use getrandom::fill as fill_random;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const MAX_LIST_ENTRIES: usize = 256;
pub const MAX_PAGE_SIZE: usize = 64;
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_LINE_COUNT: usize = 100_000;
pub const MAX_SEARCH_MATCHES: usize = 256;
pub const MAX_SEARCH_FILES: usize = 256;
pub const MAX_SEARCH_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SEARCH_DEPTH: usize = 64;
pub const MAX_CONCURRENT_OPERATIONS: usize = 8;
pub const MAX_CHUNKS_PER_READ: usize = 4_096;
pub const MAX_MUTATION_LOCKS: usize = 1_024;
pub const MAX_DIRECTORY_IDENTITIES: usize = 1_024;
pub const MAX_TOMBSTONES: usize = 64;

const TOMBSTONE_PREFIX: &str = ".devmanager-tombstone-";
#[cfg(target_os = "linux")]
const CLEANUP_AUTHORITY_ENTRY_PREFIX: &str = ".devmanager-cleanup-";
const MAX_SEARCH_QUERY_BYTES: usize = 256;
const MAX_SEARCH_ENTRIES: usize = 1_024;
const MAX_SEARCH_LINE_BYTES: usize = 256 * 1024;
const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_RELATIVE_PATH_BYTES: usize = 1024;
const MAX_RELATIVE_PATH_CHARS: usize = 1024;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_OPERATION_DURATION: Duration = Duration::from_secs(5);
// Directory search revalidates each component by identity. Keep enough
// shared work for the complete structural depth walk to reach its explicit
// overflow check; the absolute five-second deadline remains the hard bound.
const MAX_OPERATION_WORK: usize = 1_000_000;

#[cfg(test)]
pub(crate) const TEST_PAUSE_BEFORE_RENAME: usize = 1;
#[cfg(test)]
pub(crate) const TEST_PAUSE_AFTER_EXCHANGE: usize = 2;
#[cfg(test)]
pub(crate) const TEST_PAUSE_AFTER_INSTALL: usize = 3;
#[cfg(test)]
pub(crate) const TEST_PAUSE_BEFORE_UNLINK: usize = 4;
#[cfg(test)]
pub(crate) const TEST_PAUSE_BEFORE_OLD_DETACH: usize = 5;
#[cfg(test)]
pub(crate) const TEST_PAUSE_AFTER_OLD_DETACH: usize = 6;
#[cfg(test)]
pub(crate) const TEST_PAUSE_BEFORE_LINE_READ: usize = 7;
#[cfg(test)]
pub(crate) const TEST_PAUSE_BEFORE_DELETE_EFFECT: usize = 8;
#[cfg(test)]
pub(crate) const TEST_PAUSE_BEFORE_EXCHANGE: usize = 9;
#[cfg(test)]
pub(crate) const TEST_OPERATION_EXPIRED_ENTRY: usize = 1;
#[cfg(test)]
pub(crate) const TEST_OPERATION_EXPIRED_MID: usize = 2;

#[cfg(test)]
static TEST_PAUSE_STAGE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_PAUSE_READY: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_FORCE_OLD_DELETE_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_ARMED_CLEANUP_DROPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_LINE_READ_BYTES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn set_test_pause(stage: usize) {
    TEST_PAUSE_READY.store(false, Ordering::Release);
    TEST_PAUSE_STAGE.store(stage, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn test_pause_ready() -> bool {
    TEST_PAUSE_READY.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn clear_test_pause() {
    TEST_PAUSE_STAGE.store(0, Ordering::Release);
    while TEST_PAUSE_READY.swap(false, Ordering::AcqRel) {
        std::thread::yield_now();
    }
}

#[cfg(test)]
pub(crate) fn reset_test_line_read_bytes() {
    TEST_LINE_READ_BYTES.store(0, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn test_line_read_bytes() -> usize {
    TEST_LINE_READ_BYTES.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn reset_test_cleanup_drop_count() {
    TEST_ARMED_CLEANUP_DROPS.store(0, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn test_cleanup_drop_count() -> usize {
    TEST_ARMED_CLEANUP_DROPS.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn set_test_force_old_delete_failure(enabled: bool) {
    TEST_FORCE_OLD_DELETE_FAILURE.store(enabled, Ordering::Release);
}

#[cfg(test)]
fn test_pause(stage: usize) {
    if TEST_PAUSE_STAGE.load(Ordering::Acquire) != stage {
        return;
    }
    TEST_PAUSE_READY.store(true, Ordering::Release);
    while TEST_PAUSE_STAGE.load(Ordering::Acquire) == stage {
        std::thread::yield_now();
    }
    TEST_PAUSE_READY.store(false, Ordering::Release);
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn new(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RepoPath")
            .field(&"<relative-path>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretClassification {
    Ordinary,
    SecretLike,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub volume_or_device: u64,
    pub file_or_inode: u64,
}

impl fmt::Debug for FileIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileIdentity")
            .field("present", &true)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub byte_len: u64,
    pub modified_unix_nanos: Option<u128>,
    pub permission_bits: u32,
    pub identity: FileIdentity,
}

impl fmt::Debug for FileFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileFingerprint")
            .field("byte_len", &self.byte_len)
            .field("modified", &self.modified_unix_nanos.is_some())
            .field("permissions_present", &true)
            .field("identity", &self.identity)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FileRevision {
    pub fingerprint: FileFingerprint,
    pub sha256: Option<[u8; 32]>,
}

impl fmt::Debug for FileRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileRevision")
            .field("fingerprint", &self.fingerprint)
            .field("sha256_present", &self.sha256.is_some())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FileMetadata {
    pub path: RepoPath,
    pub kind: EntryKind,
    pub byte_len: Option<u64>,
    pub modified_unix_nanos: Option<u128>,
    pub permission_bits: u32,
    pub secret: SecretClassification,
    pub content_kind: Option<ContentKind>,
}

impl fmt::Debug for FileMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileMetadata")
            .field("path_present", &true)
            .field("kind", &self.kind)
            .field("byte_len", &self.byte_len)
            .field("modified", &self.modified_unix_nanos.is_some())
            .field("permissions_present", &true)
            .field("secret", &self.secret)
            .field("content_kind", &self.content_kind)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOptions {
    pub chunk_bytes: usize,
    pub total_bytes: usize,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: MAX_CHUNK_BYTES,
            total_bytes: MAX_READ_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePageRequest {
    pub offset: usize,
    pub limit: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DirectoryCursor {
    directory: Option<RepoPath>,
    revision: [u8; 32],
    service_authority: [u8; 16],
    root_identity: FileIdentity,
    directory_identity: FileIdentity,
    epoch: u64,
}

impl fmt::Debug for DirectoryCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryCursor")
            .field("directory_present", &self.directory.is_some())
            .field("revision_present", &true)
            .field("service_bound", &true)
            .field("root_bound", &true)
            .field("epoch_present", &true)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FilePage {
    pub entries: Vec<FileMetadata>,
    pub offset: usize,
    pub total_entries: usize,
    pub next_offset: Option<usize>,
    pub next_cursor: Option<DirectoryCursor>,
}

impl fmt::Debug for FilePage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FilePage")
            .field("entry_count", &self.entries.len())
            .field("offset", &self.offset)
            .field("total_entries", &self.total_entries)
            .field("has_next", &self.next_offset.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePageRequest {
    pub start_line: usize,
    pub limit: usize,
    pub expected_revision: Option<FileRevision>,
}

impl LinePageRequest {
    pub fn new(start_line: usize, limit: usize) -> Self {
        Self {
            start_line,
            limit,
            expected_revision: None,
        }
    }

    pub fn after(start_line: usize, limit: usize, revision: FileRevision) -> Self {
        Self {
            start_line,
            limit,
            expected_revision: Some(revision),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FileLine {
    pub number: usize,
    pub text: String,
}

impl fmt::Debug for FileLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileLine")
            .field("number", &self.number)
            .field("text_len", &self.text.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LinePage {
    pub path: RepoPath,
    pub lines: Vec<FileLine>,
    pub total_lines: usize,
    pub next_start_line: Option<usize>,
    pub revision: FileRevision,
}

impl fmt::Debug for LinePage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinePage")
            .field("path_present", &true)
            .field("line_count", &self.lines.len())
            .field("total_lines", &self.total_lines)
            .field("has_next", &self.next_start_line.is_some())
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    pub max_matches: usize,
    pub max_files: usize,
    pub max_bytes: usize,
    pub case_sensitive: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_matches: MAX_SEARCH_MATCHES,
            max_files: MAX_SEARCH_FILES,
            max_bytes: MAX_SEARCH_BYTES,
            case_sensitive: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: RepoPath,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

impl fmt::Debug for SearchMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchMatch")
            .field("path_present", &true)
            .field("line", &self.line)
            .field("column", &self.column)
            .field("text_len", &self.text.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    pub scanned_files: usize,
    pub scanned_bytes: usize,
}

impl fmt::Debug for SearchResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchResult")
            .field("match_count", &self.matches.len())
            .field("scanned_files", &self.scanned_files)
            .field("scanned_bytes", &self.scanned_bytes)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ReadChunk {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for ReadChunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadChunk")
            .field("offset", &self.offset)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub path: RepoPath,
    pub content_kind: ContentKind,
    pub chunks: Vec<ReadChunk>,
    pub total_bytes: u64,
    pub revision: FileRevision,
}

impl fmt::Debug for ReadResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadResult")
            .field("path_present", &true)
            .field("content_kind", &self.content_kind)
            .field("chunk_count", &self.chunks.len())
            .field("total_bytes", &self.total_bytes)
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ExpectedRevision {
    Missing,
    Exact {
        fingerprint: FileFingerprint,
        sha256: Option<[u8; 32]>,
    },
}

impl fmt::Debug for ExpectedRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("ExpectedRevision::Missing"),
            Self::Exact {
                fingerprint,
                sha256,
            } => formatter
                .debug_struct("ExpectedRevision::Exact")
                .field("fingerprint", fingerprint)
                .field("sha256_present", &sha256.is_some())
                .finish(),
        }
    }
}

impl ExpectedRevision {
    pub fn missing() -> Self {
        Self::Missing
    }

    pub fn exact(revision: FileRevision) -> Self {
        Self::Exact {
            fingerprint: revision.fingerprint,
            sha256: revision.sha256,
        }
    }

    pub fn fingerprint(fingerprint: FileFingerprint) -> Self {
        Self::Exact {
            fingerprint,
            sha256: None,
        }
    }
}

#[derive(PartialEq, Eq)]
pub struct DeletePreview {
    record: MutationRecord,
    revision: FileRevision,
    secret: SecretClassification,
}

impl fmt::Debug for DeletePreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeletePreview")
            .field("path_present", &true)
            .field("revision", &self.revision)
            .field("secret", &self.secret)
            .finish()
    }
}

impl DeletePreview {
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeleteResult {
    pub path: RepoPath,
    pub revision: FileRevision,
}

impl fmt::Debug for DeleteResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeleteResult")
            .field("path_present", &true)
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct WriteResult {
    pub path: RepoPath,
    pub bytes_written: usize,
    pub revision: FileRevision,
}

impl fmt::Debug for WriteResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WriteResult")
            .field("path_present", &true)
            .field("bytes_written", &self.bytes_written)
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum FileServiceError {
    RootUnavailable,
    AuthorityUnavailable,
    SecretLikePath,
    InvalidPath {
        path: String,
        reason: &'static str,
    },
    NotFound {
        path: String,
    },
    NotDirectory {
        path: String,
    },
    NotRegularFile {
        path: String,
    },
    OutsideWorkspace {
        path: String,
    },
    ReparseRejected {
        path: String,
    },
    HardLinkRejected {
        path: String,
    },
    ListOverflow {
        limit: usize,
    },
    PageLimitExceeded {
        limit: usize,
    },
    InvalidPageOffset {
        offset: usize,
    },
    DirectoryChanged {
        path: String,
    },
    DeadlineExceeded,
    InvalidReadOptions,
    ReadLimitExceeded {
        limit: usize,
    },
    ChunkLimitExceeded {
        limit: usize,
    },
    LineLimitExceeded {
        limit: usize,
    },
    LineTooLong {
        limit: usize,
    },
    BinaryContent {
        path: String,
    },
    WriteLimitExceeded {
        limit: usize,
    },
    InvalidSearchOptions,
    SearchLimitExceeded {
        limit: usize,
    },
    ConcurrencyLimitExceeded {
        limit: usize,
    },
    Unsupported {
        operation: &'static str,
    },
    ChangedDuringRead {
        path: String,
    },
    Conflict {
        path: String,
    },
    /// A commit detached an inode, but a subsequent identity check could not
    /// prove that cleanup still refers to that same inode. The residue is
    /// intentionally left visible for a later bounded recovery attempt.
    CleanupFailed,
    ForeignPlan,
    PermissionPreservationFailed {
        path: String,
    },
    Io {
        operation: &'static str,
        path: String,
        kind: io::ErrorKind,
        raw_code: Option<i32>,
    },
}

impl fmt::Debug for FileServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::RootUnavailable => "RootUnavailable",
            Self::AuthorityUnavailable => "AuthorityUnavailable",
            Self::SecretLikePath => "SecretLikePath",
            Self::InvalidPath { .. } => "InvalidPath",
            Self::NotFound { .. } => "NotFound",
            Self::NotDirectory { .. } => "NotDirectory",
            Self::NotRegularFile { .. } => "NotRegularFile",
            Self::OutsideWorkspace { .. } => "OutsideWorkspace",
            Self::ReparseRejected { .. } => "ReparseRejected",
            Self::HardLinkRejected { .. } => "HardLinkRejected",
            Self::ListOverflow { .. } => "ListOverflow",
            Self::PageLimitExceeded { .. } => "PageLimitExceeded",
            Self::InvalidPageOffset { .. } => "InvalidPageOffset",
            Self::DirectoryChanged { .. } => "DirectoryChanged",
            Self::DeadlineExceeded => "DeadlineExceeded",
            Self::InvalidReadOptions => "InvalidReadOptions",
            Self::ReadLimitExceeded { .. } => "ReadLimitExceeded",
            Self::ChunkLimitExceeded { .. } => "ChunkLimitExceeded",
            Self::LineLimitExceeded { .. } => "LineLimitExceeded",
            Self::LineTooLong { .. } => "LineTooLong",
            Self::BinaryContent { .. } => "BinaryContent",
            Self::WriteLimitExceeded { .. } => "WriteLimitExceeded",
            Self::InvalidSearchOptions => "InvalidSearchOptions",
            Self::SearchLimitExceeded { .. } => "SearchLimitExceeded",
            Self::ConcurrencyLimitExceeded { .. } => "ConcurrencyLimitExceeded",
            Self::Unsupported { .. } => "Unsupported",
            Self::ChangedDuringRead { .. } => "ChangedDuringRead",
            Self::Conflict { .. } => "Conflict",
            Self::CleanupFailed => "CleanupFailed",
            Self::ForeignPlan => "ForeignPlan",
            Self::PermissionPreservationFailed { .. } => "PermissionPreservationFailed",
            Self::Io { .. } => "Io",
        };
        formatter.debug_struct(name).finish()
    }
}

impl fmt::Display for FileServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnavailable => write!(formatter, "workspace root is unavailable"),
            Self::AuthorityUnavailable => write!(formatter, "workspace authority is unavailable"),
            Self::SecretLikePath => write!(formatter, "workspace path is secret-like"),
            Self::InvalidPath { reason, .. } => {
                write!(formatter, "invalid workspace-relative path: {reason}")
            }
            Self::NotFound { .. } => write!(formatter, "workspace target not found"),
            Self::NotDirectory { .. } => {
                write!(formatter, "workspace target is not a directory")
            }
            Self::NotRegularFile { .. } => {
                write!(formatter, "workspace target is not a regular file")
            }
            Self::OutsideWorkspace { .. } => {
                write!(formatter, "workspace target is outside the bound root")
            }
            Self::ReparseRejected { .. } => {
                write!(formatter, "workspace reparse target rejected")
            }
            Self::HardLinkRejected { .. } => {
                write!(formatter, "workspace hardlink rejected")
            }
            Self::ListOverflow { limit } => {
                write!(
                    formatter,
                    "workspace listing exceeds the {limit}-entry limit"
                )
            }
            Self::PageLimitExceeded { limit } => {
                write!(formatter, "workspace page exceeds the {limit}-entry limit")
            }
            Self::InvalidPageOffset { offset } => {
                write!(formatter, "workspace page offset is invalid: {offset}")
            }
            Self::DirectoryChanged { .. } => write!(formatter, "workspace directory changed"),
            Self::DeadlineExceeded => write!(formatter, "workspace operation deadline exceeded"),
            Self::InvalidReadOptions => {
                write!(
                    formatter,
                    "workspace read options exceed the bounded contract"
                )
            }
            Self::ReadLimitExceeded { limit } => {
                write!(formatter, "workspace read exceeds the {limit}-byte limit")
            }
            Self::ChunkLimitExceeded { limit } => {
                write!(formatter, "workspace read exceeds the {limit}-chunk limit")
            }
            Self::LineLimitExceeded { limit } => {
                write!(formatter, "workspace text exceeds the {limit}-line limit")
            }
            Self::LineTooLong { limit } => {
                write!(
                    formatter,
                    "workspace text line exceeds the {limit}-byte limit"
                )
            }
            Self::BinaryContent { .. } => {
                write!(formatter, "workspace file is not valid UTF-8 text")
            }
            Self::WriteLimitExceeded { limit } => {
                write!(formatter, "workspace write exceeds the {limit}-byte limit")
            }
            Self::InvalidSearchOptions => {
                write!(
                    formatter,
                    "workspace search options exceed the bounded contract"
                )
            }
            Self::SearchLimitExceeded { limit } => {
                write!(formatter, "workspace search exceeds the {limit}-item limit")
            }
            Self::ConcurrencyLimitExceeded { limit } => write!(
                formatter,
                "workspace file operation concurrency exceeds the {limit}-operation limit"
            ),
            Self::Unsupported { operation } => {
                write!(
                    formatter,
                    "workspace {operation} is unsupported on this platform"
                )
            }
            Self::ChangedDuringRead { .. } => {
                write!(formatter, "workspace file changed during read")
            }
            Self::Conflict { .. } => write!(formatter, "workspace file changed since preview"),
            Self::CleanupFailed => write!(
                formatter,
                "workspace cleanup could not be proven; durable residue was retained"
            ),
            Self::ForeignPlan => {
                write!(formatter, "workspace mutation plan belongs to another root")
            }
            Self::PermissionPreservationFailed { .. } => {
                write!(
                    formatter,
                    "workspace file permissions could not be preserved"
                )
            }
            Self::Io {
                operation,
                kind,
                raw_code,
                ..
            } => {
                write!(formatter, "workspace {operation} failed: {kind:?}")?;
                if let Some(raw_code) = raw_code {
                    write!(formatter, " (os error {raw_code})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for FileServiceError {}

#[derive(Clone)]
struct OperationDeadline {
    expires_at: Instant,
    remaining_work: Arc<AtomicUsize>,
}

impl OperationDeadline {
    fn new() -> Self {
        Self {
            expires_at: Instant::now() + MAX_OPERATION_DURATION,
            remaining_work: Arc::new(AtomicUsize::new(MAX_OPERATION_WORK)),
        }
    }

    fn with_work(work: usize) -> Self {
        Self {
            expires_at: Instant::now() + MAX_OPERATION_DURATION,
            remaining_work: Arc::new(AtomicUsize::new(work)),
        }
    }

    #[cfg(test)]
    fn with_duration(duration: Duration) -> Self {
        Self {
            expires_at: Instant::now() + duration,
            remaining_work: Arc::new(AtomicUsize::new(MAX_OPERATION_WORK)),
        }
    }

    fn check(&self) -> Result<(), FileServiceError> {
        if Instant::now() >= self.expires_at {
            return Err(FileServiceError::DeadlineExceeded);
        }
        let mut remaining = self.remaining_work.load(Ordering::Acquire);
        loop {
            if remaining == 0 {
                return Err(FileServiceError::DeadlineExceeded);
            }
            match self.remaining_work.compare_exchange_weak(
                remaining,
                remaining - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => remaining = observed,
            }
        }
    }

    fn remaining(&self) -> Result<Duration, FileServiceError> {
        let now = Instant::now();
        if now >= self.expires_at {
            return Err(FileServiceError::DeadlineExceeded);
        }
        Ok(self.expires_at.duration_since(now))
    }
}

/// A mutex whose wait is bounded by the caller's absolute operation deadline.
/// The atomic owner bit keeps the condition-variable wait independent from the
/// protected value, so a waiter never blocks indefinitely acquiring the state
/// mutex itself. Drop paths use `try_lock` and therefore never wait.
struct DeadlineMutex<T> {
    locked: std::sync::atomic::AtomicBool,
    wait: Mutex<()>,
    wake: Condvar,
    value: Mutex<T>,
}

impl<T> DeadlineMutex<T> {
    fn new(value: T) -> Self {
        Self {
            locked: std::sync::atomic::AtomicBool::new(false),
            wait: Mutex::new(()),
            wake: Condvar::new(),
            value: Mutex::new(value),
        }
    }

    fn release(&self) {
        self.locked.store(false, Ordering::Release);
        self.wake.notify_one();
    }

    fn lock_until<'a>(
        &'a self,
        deadline: &OperationDeadline,
    ) -> Result<DeadlineMutexGuard<'a, T>, FileServiceError> {
        deadline.check()?;
        if self
            .locked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // A different waiter may currently own the condition-variable
            // mutex. Never queue behind that mutex with an unbounded
            // `lock()`: park once for this operation's remaining absolute
            // budget and return a typed deadline if the owner does not hand
            // the condition variable back first. This avoids both short
            // polling and a second wait whose lifetime can exceed ours.
            let mut waiter = loop {
                match self.wait.try_lock() {
                    Ok(waiter) => break waiter,
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err(FileServiceError::RootUnavailable)
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        let remaining = deadline.remaining()?;
                        std::thread::park_timeout(remaining);
                        deadline.check()?;
                    }
                }
            };
            loop {
                if !self.locked.load(Ordering::Acquire)
                    && self
                        .locked
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    break;
                }
                let remaining = deadline.remaining()?;
                let (next_waiter, timeout) = self
                    .wake
                    .wait_timeout(waiter, remaining)
                    .map_err(|_| FileServiceError::RootUnavailable)?;
                waiter = next_waiter;
                if timeout.timed_out() {
                    deadline.check()?;
                    return Err(FileServiceError::DeadlineExceeded);
                }
                deadline.check()?;
            }
        }
        let value = match self.value.lock() {
            Ok(value) => value,
            Err(_) => {
                self.release();
                return Err(FileServiceError::RootUnavailable);
            }
        };
        Ok(DeadlineMutexGuard {
            owner: self,
            value: Some(value),
        })
    }

    fn try_lock(&self) -> Option<DeadlineMutexGuard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let value = match self.value.lock() {
            Ok(value) => value,
            Err(_) => {
                self.release();
                return None;
            }
        };
        Some(DeadlineMutexGuard {
            owner: self,
            value: Some(value),
        })
    }
}

struct DeadlineMutexGuard<'a, T> {
    owner: &'a DeadlineMutex<T>,
    value: Option<std::sync::MutexGuard<'a, T>>,
}

impl<T> Deref for DeadlineMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
            .as_deref()
            .expect("deadline mutex guard must hold its value")
    }
}

impl<T> DerefMut for DeadlineMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
            .as_deref_mut()
            .expect("deadline mutex guard must hold its value")
    }
}

impl<T> Drop for DeadlineMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.value.take();
        self.owner.release();
    }
}

#[cfg(test)]
pub(crate) fn deadline_lock_times_out_for_test() -> bool {
    let lock = DeadlineMutex::new(());
    let guard = lock
        .lock_until(&OperationDeadline::new())
        .expect("test lock owner");
    let started = Instant::now();
    let result = lock.lock_until(&OperationDeadline::with_duration(Duration::from_millis(20)));
    drop(guard);
    matches!(result, Err(FileServiceError::DeadlineExceeded))
        && started.elapsed() < Duration::from_secs(1)
}

struct OpenedRoot {
    path: PathBuf,
    handle: File,
    write_handle: Option<File>,
    identity: FileIdentity,
    name_marker: RootNameMarker,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RootNameMarker {
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    file_attributes: u32,
    #[cfg(unix)]
    identity: FileIdentity,
}

struct ApprovedWorkspaceRoot {
    root: OpenedRoot,
}

impl fmt::Debug for ApprovedWorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovedWorkspaceRoot")
            .field("bound", &true)
            .finish()
    }
}

/// Opaque host-issued scope for one file-service binding.
///
/// This type intentionally has no constructor. The Task 6.2 bridge is the
/// only production issuer and supplies all lease/task/client/connection/epoch
/// dimensions after authenticating the request.
#[derive(Clone, Copy, PartialEq, Eq)]
struct WorkspaceFileAuthority {
    workspace_lease: [u8; 16],
    task_id: [u8; 16],
    client_id: [u8; 16],
    connection_id: [u8; 16],
    action_epoch: u64,
}

impl fmt::Debug for WorkspaceFileAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceFileAuthority(REDACTED)")
    }
}

pub(crate) mod task6_bridge {
    pub(crate) trait Sealed {}
}

/// Opaque production resource-lease holder. Test fixtures omit it.
pub(crate) trait Task6LiveLeaseGuard: Send + Sync {
    fn ensure_active(&self) -> bool;
}

pub(crate) struct OpaqueTask6LeaseGuard {
    inner: Option<Box<dyn Task6LiveLeaseGuard>>,
}

impl Default for OpaqueTask6LeaseGuard {
    fn default() -> Self {
        Self { inner: None }
    }
}

impl fmt::Debug for OpaqueTask6LeaseGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueTask6LeaseGuard(REDACTED)")
    }
}

impl OpaqueTask6LeaseGuard {
    pub(crate) fn from_live(guard: Box<dyn Task6LiveLeaseGuard>) -> Self {
        Self { inner: Some(guard) }
    }

    fn is_live(&self) -> bool {
        self.inner
            .as_ref()
            .map(|guard| guard.ensure_active())
            .unwrap_or(true)
    }
}

/// Crate-private integration seam owned by the future Task 6.2 workspace
/// binder. The issuer passes the retained final handle it verified while
/// binding the workspace; this service never reopens the root by path. The
/// returned binding type is private, so callers cannot mint a lease, authority,
/// or root from raw IDs or a path.
pub(crate) trait Task6WorkspaceLease: task6_bridge::Sealed {
    /// The path is diagnostic/fallback metadata only; it is never reopened to
    /// establish authority after this bridge call.
    fn retained_root_path(&self) -> &Path;
    /// Task 6.2 must return the final directory handle it already verified,
    /// including all ancestor/reparse checks.
    fn retained_root_handle(&self) -> &File;
    fn retained_root_write_handle(&self) -> Option<&File>;
    fn workspace_lease(&self) -> [u8; 16];
    fn task_id(&self) -> [u8; 16];
    fn client_id(&self) -> [u8; 16];
    fn connection_id(&self) -> [u8; 16];
    fn action_epoch(&self) -> u64;

    fn take_live_lease_guard(&mut self) -> OpaqueTask6LeaseGuard {
        OpaqueTask6LeaseGuard::default()
    }

    fn into_file_binding(mut self) -> Result<Task6FileBinding, FileServiceError>
    where
        Self: Sized,
    {
        let lease_guard = self.take_live_lease_guard();
        let path = self.retained_root_path().to_path_buf();
        let handle = self
            .retained_root_handle()
            .try_clone()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        let write_handle = self
            .retained_root_write_handle()
            .map(File::try_clone)
            .transpose()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        let metadata = handle
            .metadata()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
            return Err(FileServiceError::RootUnavailable);
        }
        let (identity, _) =
            opened_file_info(&handle).map_err(|_| FileServiceError::RootUnavailable)?;
        if let Some(write_handle) = write_handle.as_ref() {
            let write_metadata = write_handle
                .metadata()
                .map_err(|_| FileServiceError::RootUnavailable)?;
            let (write_identity, _) =
                opened_file_info(write_handle).map_err(|_| FileServiceError::RootUnavailable)?;
            if !write_metadata.is_dir()
                || metadata_is_reparse_point(&write_metadata)
                || write_identity != identity
            {
                return Err(FileServiceError::RootUnavailable);
            }
        }
        Ok(Task6FileBinding {
            root: ApprovedWorkspaceRoot {
                root: OpenedRoot {
                    path,
                    handle,
                    write_handle,
                    identity,
                    name_marker: root_name_marker(&metadata),
                },
            },
            authority: WorkspaceFileAuthority {
                workspace_lease: self.workspace_lease(),
                task_id: self.task_id(),
                client_id: self.client_id(),
                connection_id: self.connection_id(),
                action_epoch: self.action_epoch(),
            },
            lease_guard,
        })
    }
}

pub(crate) struct Task6FileBinding {
    root: ApprovedWorkspaceRoot,
    authority: WorkspaceFileAuthority,
    lease_guard: OpaqueTask6LeaseGuard,
}

#[cfg(test)]
struct TestTask6WorkspaceLease {
    root: ApprovedWorkspaceRoot,
    variant: u8,
}

#[cfg(test)]
impl task6_bridge::Sealed for TestTask6WorkspaceLease {}

#[cfg(test)]
impl Task6WorkspaceLease for TestTask6WorkspaceLease {
    fn retained_root_path(&self) -> &Path {
        &self.root.root.path
    }

    fn retained_root_handle(&self) -> &File {
        &self.root.root.handle
    }

    fn retained_root_write_handle(&self) -> Option<&File> {
        self.root.root.write_handle.as_ref()
    }

    fn workspace_lease(&self) -> [u8; 16] {
        if self.variant == 1 {
            [9_u8; 16]
        } else {
            [1_u8; 16]
        }
    }

    fn task_id(&self) -> [u8; 16] {
        if self.variant == 2 {
            [9_u8; 16]
        } else {
            [2_u8; 16]
        }
    }

    fn client_id(&self) -> [u8; 16] {
        if self.variant == 3 {
            [9_u8; 16]
        } else {
            [3_u8; 16]
        }
    }

    fn connection_id(&self) -> [u8; 16] {
        if self.variant == 4 {
            [9_u8; 16]
        } else {
            [4_u8; 16]
        }
    }

    fn action_epoch(&self) -> u64 {
        if self.variant == 5 {
            2
        } else {
            1
        }
    }
}

#[cfg(test)]
pub(crate) fn task6_bridge_retained_handle_swap_proof_for_test() -> bool {
    let Ok(temp) = tempfile::tempdir() else {
        return false;
    };
    let workspace = temp.path().join("workspace");
    if fs::create_dir(&workspace).is_err() {
        return false;
    }
    let Ok(opened) = open_approved_root(&workspace) else {
        return false;
    };
    let original_identity = opened.identity;
    let moved = temp.path().join("workspace-moved");
    if fs::rename(&workspace, &moved).is_err() || fs::create_dir(&workspace).is_err() {
        return false;
    }
    let lease = TestTask6WorkspaceLease {
        root: ApprovedWorkspaceRoot { root: opened },
        variant: 0,
    };
    let Ok(binding) = lease.into_file_binding() else {
        return false;
    };
    let Ok((bound_identity, _)) = opened_file_info(&binding.root.root.handle) else {
        return false;
    };
    let Ok(replacement) = open_nofollow(&workspace, true, false) else {
        return false;
    };
    let Ok((replacement_identity, _)) = opened_file_info(&replacement) else {
        return false;
    };
    bound_identity == original_identity && bound_identity != replacement_identity
}

#[derive(Clone)]
struct MutationRecord {
    service_authority: [u8; 16],
    authority: WorkspaceFileAuthority,
    path: RepoPath,
    expected: ExpectedRevision,
    commit_revision: Option<FileRevision>,
    target_identity: Option<FileIdentity>,
    parent_identity: FileIdentity,
}

impl PartialEq for MutationRecord {
    fn eq(&self, other: &Self) -> bool {
        self.service_authority == other.service_authority
            && self.authority == other.authority
            && self.path == other.path
            && self.expected == other.expected
            && self.commit_revision == other.commit_revision
            && self.target_identity == other.target_identity
            && self.parent_identity == other.parent_identity
    }
}

impl Eq for MutationRecord {}

#[allow(dead_code)]
struct TombstoneRecord {
    parent: File,
    parent_identity: FileIdentity,
    expected_parent_identity: FileIdentity,
    name: String,
    identity: FileIdentity,
    expected_target_identity: Option<FileIdentity>,
    operation_nonce: [u8; 16],
    uncertain: bool,
    recovering: bool,
}

fn cleanup_name_binding(name: &str) -> Option<CleanupNameBinding> {
    parse_tombstone_binding(name).or_else(|| parse_temporary_binding(name))
}

fn cleanup_operation_nonce(name: &str, identity: FileIdentity) -> [u8; 16] {
    if let Some(binding) = cleanup_name_binding(name) {
        return binding.operation_nonce;
    }
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(identity.volume_or_device.to_le_bytes());
    hasher.update(identity.file_or_inode.to_le_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..16]
        .try_into()
        .expect("digest prefix has fixed size")
}

fn cleanup_expected_target(name: &str) -> Option<FileIdentity> {
    cleanup_name_binding(name).and_then(|binding| binding.expected_target_identity)
}

fn cleanup_binding_matches_identity(
    _name: &str,
    binding: CleanupNameBinding,
    identity: FileIdentity,
) -> bool {
    // The identity field in every generated name is the exact inode created
    // by this operation.  The expected-target field is context, never a
    // fallback authorization: accepting it would let a forged/relabelled
    // temporary name adopt a different inode after restart.
    binding.identity == identity
}

/// One bounded authority covers explicit operation reservations and both RAII
/// cleanup guards. A guard may commit a slot into the durable record list or
/// convert it into uncertainty, but it can never create a residue outside the
/// 64-entry cap.
struct CleanupLedger {
    tombstones: DeadlineMutex<Vec<TombstoneRecord>>,
    occupied: AtomicUsize,
    uncertain_cleanups: AtomicUsize,
}

impl CleanupLedger {
    fn new() -> Self {
        Self {
            tombstones: DeadlineMutex::new(Vec::new()),
            occupied: AtomicUsize::new(0),
            uncertain_cleanups: AtomicUsize::new(0),
        }
    }

    fn try_reserve(&self) -> bool {
        let mut occupied = self.occupied.load(Ordering::Acquire);
        loop {
            if occupied >= MAX_TOMBSTONES {
                return false;
            }
            match self.occupied.compare_exchange_weak(
                occupied,
                occupied + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => occupied = observed,
            }
        }
    }

    fn release(&self) {
        self.occupied.fetch_sub(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn occupied(&self) -> usize {
        self.occupied.load(Ordering::Acquire)
    }
}

fn same_cleanup_slot(existing: &TombstoneRecord, candidate: &TombstoneRecord) -> bool {
    existing.parent_identity == candidate.parent_identity
        && existing.name == candidate.name
        && existing.identity == candidate.identity
}

fn same_cleanup_binding(existing: &TombstoneRecord, candidate: &TombstoneRecord) -> bool {
    same_cleanup_slot(existing, candidate)
        && existing.expected_parent_identity == candidate.expected_parent_identity
        && existing.expected_target_identity == candidate.expected_target_identity
        && existing.operation_nonce == candidate.operation_nonce
}

fn reserve_cleanup_slot(accounting: &Arc<CleanupLedger>) -> Option<TombstoneReservation> {
    accounting.try_reserve().then(|| TombstoneReservation {
        ledger: Arc::clone(accounting),
        released: false,
    })
}

fn insert_reserved_cleanup_record(
    accounting: &CleanupLedger,
    reservation: &mut TombstoneReservation,
    record: TombstoneRecord,
    deadline: &OperationDeadline,
) -> Result<(), FileServiceError> {
    deadline.check()?;
    let mut tombstones = accounting.tombstones.lock_until(deadline)?;
    deadline.check()?;
    if let Some(existing) = tombstones
        .iter_mut()
        .find(|existing| same_cleanup_slot(existing, &record))
    {
        if !same_cleanup_binding(existing, &record) {
            reservation.release();
            return Err(FileServiceError::CleanupFailed);
        }
        if record.uncertain && !existing.uncertain {
            existing.uncertain = true;
            accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
        }
        reservation.release();
    } else {
        if record.uncertain {
            accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
        }
        tombstones.push(record);
        reservation.commit();
    }
    deadline.check()?;
    Ok(())
}

fn insert_reserved_cleanup_record_nonblocking(
    accounting: &CleanupLedger,
    reservation: &mut TombstoneReservation,
    record: TombstoneRecord,
) {
    let Some(mut tombstones) = accounting.tombstones.try_lock() else {
        reservation.release();
        return;
    };
    if let Some(existing) = tombstones
        .iter_mut()
        .find(|existing| same_cleanup_slot(existing, &record))
    {
        if same_cleanup_binding(existing, &record) {
            if record.uncertain && !existing.uncertain {
                existing.uncertain = true;
                accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
            }
        }
        // A conflicting equal-slot observation is deliberately not adopted;
        // the exact private residue remains visible under the existing
        // identity-bound record while this reservation is relinquished.
        reservation.release();
    } else {
        if record.uncertain {
            accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
        }
        tombstones.push(record);
        reservation.commit();
    }
}

fn persist_cleanup_record(
    accounting: &CleanupLedger,
    reservation: &mut TombstoneReservation,
    record: TombstoneRecord,
) {
    if reservation.released {
        // A recovery record owns the slot already. Its path/identity is
        // updated in-place by the post-move transfer helper; inserting here
        // would create a 65th entry at capacity.
        return;
    }
    insert_reserved_cleanup_record_nonblocking(accounting, reservation, record);
}

#[cfg(target_os = "linux")]
fn update_cleanup_record_after_move(
    tombstones: &mut Vec<TombstoneRecord>,
    old_parent_identity: FileIdentity,
    old_name: &str,
    old_identity: FileIdentity,
    current_parent: File,
    current_parent_identity: FileIdentity,
    current_name: &str,
    current_identity: FileIdentity,
) -> Option<bool> {
    let Some(record) = tombstones.iter_mut().find(|record| {
        record.parent_identity == old_parent_identity
            && record.name == old_name
            && record.identity == old_identity
    }) else {
        return None;
    };
    record.parent = current_parent;
    record.parent_identity = current_parent_identity;
    // Preserve the original workspace parent binding while the current
    // descriptor moves into the process-private authority. The name carries
    // that original parent identity so restart discovery cannot adopt an
    // entry created for another workspace.
    record.name = current_name.to_string();
    record.identity = current_identity;
    // Preserve an explicit missing-target marker on a new-file temporary;
    // authority names encode the current inode as their target binding.
    record.expected_target_identity =
        cleanup_expected_target(current_name).or(Some(current_identity));
    let newly_uncertain = if !record.uncertain {
        record.uncertain = true;
        true
    } else {
        false
    };
    Some(newly_uncertain)
}

fn insert_reserved_cleanup_record_nonblocking_from_parts(
    accounting: &CleanupLedger,
    reservation: &mut TombstoneReservation,
    parent: &File,
    parent_identity: FileIdentity,
    name: String,
    identity: FileIdentity,
    uncertain: bool,
) {
    let Ok(parent) = parent.try_clone() else {
        reservation.release();
        return;
    };
    let expected_target_identity = cleanup_expected_target(&name);
    let operation_nonce = cleanup_operation_nonce(&name, identity);
    insert_reserved_cleanup_record_nonblocking(
        accounting,
        reservation,
        TombstoneRecord {
            parent,
            parent_identity,
            expected_parent_identity: parent_identity,
            name,
            identity,
            expected_target_identity,
            operation_nonce,
            uncertain,
            recovering: false,
        },
    );
}

/// Drop paths use this nonblocking path. If the bounded ledger is full or
/// contended, no slot is consumed; the exact private name remains visible for
/// the next startup scan to discover and bind with a fresh reservation.
fn record_uncertain_cleanup_nonblocking(
    accounting: &Arc<CleanupLedger>,
    parent: &File,
    parent_identity: FileIdentity,
    name: &str,
    identity: FileIdentity,
) {
    if !is_private_cleanup_name(name) {
        return;
    }
    let Some(mut reservation) = reserve_cleanup_slot(accounting) else {
        return;
    };
    let parent = match parent.try_clone() {
        Ok(parent) => parent,
        Err(_) => {
            reservation.release();
            return;
        }
    };
    let mut tombstones = match accounting.tombstones.try_lock() {
        Some(tombstones) => tombstones,
        None => {
            reservation.release();
            return;
        }
    };
    let candidate = TombstoneRecord {
        parent,
        parent_identity,
        expected_parent_identity: parent_identity,
        name: name.to_string(),
        identity,
        expected_target_identity: cleanup_expected_target(name),
        operation_nonce: cleanup_operation_nonce(name, identity),
        uncertain: true,
        recovering: false,
    };
    if let Some(existing) = tombstones
        .iter_mut()
        .find(|record| same_cleanup_slot(record, &candidate))
    {
        if same_cleanup_binding(existing, &candidate) {
            if !existing.uncertain {
                existing.uncertain = true;
                accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
            }
        }
        // A same-slot metadata conflict is fail-closed: release only the
        // speculative reservation and leave the pre-existing exact record.
        reservation.release();
    } else {
        accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
        tombstones.push(candidate);
        reservation.commit();
    }
}

pub struct WritePlan {
    record: MutationRecord,
    contents: Vec<u8>,
    secret: SecretClassification,
}

impl fmt::Debug for WritePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WritePlan")
            .field("path_present", &true)
            .field("content_len", &self.contents.len())
            .field("expected", &self.record.expected)
            .field("secret", &self.secret)
            .finish()
    }
}

pub struct WorkspaceFileService {
    root: OpenedRoot,
    authority: [u8; 16],
    binding: WorkspaceFileAuthority,
    cursor_epoch: u64,
    active_operations: Arc<AtomicUsize>,
    mutation_locks: DeadlineMutex<HashMap<String, Arc<DeadlineMutex<()>>>>,
    directory_identities: DeadlineMutex<HashMap<String, FileIdentity>>,
    directory_identity_order: DeadlineMutex<VecDeque<String>>,
    cleanup: Arc<CleanupLedger>,
    lease_guard: OpaqueTask6LeaseGuard,
    #[cfg(test)]
    test_budget_mode: AtomicUsize,
    #[cfg(test)]
    root_revalidations: AtomicUsize,
}

struct TombstoneReservation {
    ledger: Arc<CleanupLedger>,
    released: bool,
}

impl TombstoneReservation {
    fn release(&mut self) {
        if !self.released {
            self.ledger.release();
            self.released = true;
        }
    }

    fn commit(&mut self) {
        self.released = true;
    }
}

impl Drop for TombstoneReservation {
    fn drop(&mut self) {
        self.release();
    }
}

impl fmt::Debug for WorkspaceFileService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceFileService")
            .field("bound", &true)
            .field("operation_limit", &MAX_CONCURRENT_OPERATIONS)
            .finish()
    }
}

pub struct FileOperationPermit {
    active_operations: Arc<AtomicUsize>,
}

impl Drop for FileOperationPermit {
    fn drop(&mut self) {
        self.active_operations.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WorkspaceFileService {
    /// Task 6.2 is the sole production issuer of an approved root and binding.
    /// The private binding return type prevents path-only or raw-ID issuance by
    /// other callers until that integration is implemented.
    pub(crate) fn from_task6_workspace<L: Task6WorkspaceLease>(
        lease: L,
    ) -> Result<Self, FileServiceError> {
        let binding = lease.into_file_binding()?;
        Self::from_task6_binding(binding)
    }

    /// Issue a least-authority service for browsing and reading. Read-only
    /// callers must not depend on the workspace-wide mutation recovery scan:
    /// large repositories can exceed that scan's deliberate safety bound even
    /// though a bounded list or read is still safe and fully authorized.
    pub(crate) fn from_task6_read_workspace<L: Task6WorkspaceLease>(
        lease: L,
    ) -> Result<Self, FileServiceError> {
        let mut binding = lease.into_file_binding()?;
        binding.root.root.write_handle = None;
        Self::from_task6_binding_unrecovered(binding)
    }

    fn from_task6_binding(binding: Task6FileBinding) -> Result<Self, FileServiceError> {
        let service = Self::from_task6_binding_unrecovered(binding)?;
        let deadline = service.operation_deadline();
        deadline.check()?;
        #[cfg(target_os = "linux")]
        discover_cleanup_authority(&deadline).map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                FileServiceError::DeadlineExceeded
            } else if error
                .to_string()
                .contains("cleanup authority scan exceeded bound")
            {
                FileServiceError::ConcurrencyLimitExceeded {
                    limit: MAX_SEARCH_ENTRIES,
                }
            } else if error
                .to_string()
                .contains("cleanup authority capacity exceeded")
            {
                FileServiceError::ConcurrencyLimitExceeded {
                    limit: MAX_TOMBSTONES,
                }
            } else {
                FileServiceError::RootUnavailable
            }
        })?;
        service.discover_tombstones(&deadline)?;
        Ok(service)
    }

    fn from_task6_binding_unrecovered(binding: Task6FileBinding) -> Result<Self, FileServiceError> {
        let mut authority = [0_u8; 16];
        fill_random(&mut authority).map_err(|_| FileServiceError::AuthorityUnavailable)?;
        let mut epoch_bytes = [0_u8; 8];
        fill_random(&mut epoch_bytes).map_err(|_| FileServiceError::AuthorityUnavailable)?;
        let cursor_epoch = u64::from_le_bytes(epoch_bytes).max(1);
        Ok(Self {
            root: binding.root.root,
            authority,
            binding: binding.authority,
            cursor_epoch,
            active_operations: Arc::new(AtomicUsize::new(0)),
            mutation_locks: DeadlineMutex::new(HashMap::new()),
            directory_identities: DeadlineMutex::new(HashMap::new()),
            directory_identity_order: DeadlineMutex::new(VecDeque::new()),
            cleanup: Arc::new(CleanupLedger::new()),
            lease_guard: binding.lease_guard,
            #[cfg(test)]
            test_budget_mode: AtomicUsize::new(0),
            #[cfg(test)]
            root_revalidations: AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn new_for_test(root: impl AsRef<Path>) -> Result<Self, FileServiceError> {
        let opened = open_approved_root(root.as_ref())?;
        Self::from_task6_workspace(TestTask6WorkspaceLease {
            root: ApprovedWorkspaceRoot { root: opened },
            variant: 0,
        })
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn new_for_test_with_authority_dimension(
        root: impl AsRef<Path>,
        variant: u8,
    ) -> Result<Self, FileServiceError> {
        let opened = open_approved_root(root.as_ref())?;
        Self::from_task6_workspace(TestTask6WorkspaceLease {
            root: ApprovedWorkspaceRoot { root: opened },
            variant,
        })
    }

    pub fn try_acquire_operation(&self) -> Result<FileOperationPermit, FileServiceError> {
        // Every public file operation must retain the live Task6 resource
        // lease for its entire duration. Test fixtures use the no-op guard;
        // production bindings fail closed as soon as the host lease is
        // revoked or released.
        if !self.lease_guard.is_live() {
            return Err(FileServiceError::AuthorityUnavailable);
        }
        let mut active = self.active_operations.load(Ordering::Acquire);
        loop {
            if active >= MAX_CONCURRENT_OPERATIONS {
                return Err(FileServiceError::ConcurrencyLimitExceeded {
                    limit: MAX_CONCURRENT_OPERATIONS,
                });
            }
            match self.active_operations.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(FileOperationPermit {
                        active_operations: Arc::clone(&self.active_operations),
                    })
                }
                Err(observed) => active = observed,
            }
        }
    }

    fn operation_deadline(&self) -> OperationDeadline {
        #[cfg(test)]
        {
            return match self.test_budget_mode.load(Ordering::Acquire) {
                TEST_OPERATION_EXPIRED_ENTRY => OperationDeadline::with_work(0),
                // Enough budget to cross validation and temporary-file setup,
                // but not enough to reach the commit/recovery tail. This
                // makes mid-operation timeout tests exercise visible cleanup
                // state instead of only the entry guard.
                TEST_OPERATION_EXPIRED_MID => OperationDeadline::with_work(96),
                _ => OperationDeadline::new(),
            };
        }
        #[cfg(not(test))]
        {
            OperationDeadline::new()
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_budget_mode(&self, mode: usize) {
        self.test_budget_mode.store(mode, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn reset_root_revalidations_for_test(&self) {
        self.root_revalidations.store(0, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn root_revalidation_count_for_test(&self) -> usize {
        self.root_revalidations.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn cleanup_occupancy_for_test(&self) -> usize {
        self.cleanup.occupied()
    }

    #[cfg(test)]
    pub(crate) fn root_identity_for_test(&self) -> FileIdentity {
        self.root.identity
    }

    #[cfg(test)]
    pub(crate) fn reserve_cleanup_capacity_for_test(&self, attempts: usize) -> (usize, usize) {
        let mut reservations = Vec::new();
        for _ in 0..attempts {
            let Ok(mut reservation) = self.reserve_tombstone_slot() else {
                break;
            };
            reservation.commit();
            reservations.push(reservation);
        }
        (reservations.len(), self.cleanup.occupied())
    }

    pub fn normalize_relative_path(&self, raw: &str) -> Result<RepoPath, FileServiceError> {
        normalize_relative_path(raw)
    }

    pub fn list(
        &self,
        relative_directory: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FileMetadata>, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        if limit == 0 || limit > MAX_LIST_ENTRIES {
            return Err(FileServiceError::ListOverflow { limit });
        }
        let directory = relative_directory
            .map(normalize_relative_path)
            .transpose()?;
        let snapshot = self.directory_snapshot_path_with_deadline(directory.as_ref(), &deadline)?;
        if snapshot.entries.len() > limit {
            return Err(FileServiceError::ListOverflow { limit });
        }
        self.revalidate_root_with_deadline(&deadline)?;
        deadline.check()?;
        Ok(snapshot.entries)
    }

    pub fn list_page(
        &self,
        relative_directory: Option<&str>,
        request: FilePageRequest,
    ) -> Result<FilePage, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        self.list_page_inner(relative_directory, request, None)
    }

    pub fn list_page_with_cursor(
        &self,
        relative_directory: Option<&str>,
        cursor: DirectoryCursor,
        request: FilePageRequest,
    ) -> Result<FilePage, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        self.list_page_inner(relative_directory, request, Some(cursor))
    }

    fn list_page_inner(
        &self,
        relative_directory: Option<&str>,
        request: FilePageRequest,
        cursor: Option<DirectoryCursor>,
    ) -> Result<FilePage, FileServiceError> {
        let deadline = self.operation_deadline();
        deadline.check()?;
        if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
            return Err(FileServiceError::PageLimitExceeded {
                limit: MAX_PAGE_SIZE,
            });
        }
        let directory = relative_directory
            .map(normalize_relative_path)
            .transpose()?;
        let snapshot = self.directory_snapshot_path_with_deadline(directory.as_ref(), &deadline)?;
        if let Some(cursor) = cursor {
            if cursor.directory != directory
                || cursor.revision != snapshot.revision
                || cursor.service_authority != self.authority
                || cursor.root_identity != self.root.identity
                || cursor.epoch != self.cursor_epoch
                || cursor.directory_identity != snapshot.identity
            {
                return Err(FileServiceError::DirectoryChanged {
                    path: directory
                        .as_ref()
                        .map_or_else(|| "<root>".to_string(), safe_path),
                });
            }
        }
        let total_entries = snapshot.entries.len();
        if request.offset > total_entries {
            return Err(FileServiceError::InvalidPageOffset {
                offset: request.offset,
            });
        }
        let end = request
            .offset
            .saturating_add(request.limit)
            .min(total_entries);
        let next_cursor = (end < total_entries).then(|| DirectoryCursor {
            directory,
            revision: snapshot.revision,
            service_authority: self.authority,
            root_identity: self.root.identity,
            directory_identity: snapshot.identity,
            epoch: self.cursor_epoch,
        });
        self.revalidate_root_with_deadline(&deadline)?;
        deadline.check()?;
        Ok(FilePage {
            entries: snapshot.entries[request.offset..end].to_vec(),
            offset: request.offset,
            total_entries,
            next_offset: (end < total_entries).then_some(end),
            next_cursor,
        })
    }

    pub fn read(
        &self,
        raw_path: &str,
        options: ReadOptions,
    ) -> Result<ReadResult, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        self.read_inner_with_deadline(raw_path, options, &deadline)
    }

    pub(crate) fn classify_secret_path(path: &str) -> SecretClassification {
        classify_secret(path)
    }

    pub(crate) fn bounded_utf8_prefix(bytes: &[u8], max_bytes: usize) -> Option<String> {
        let end = bytes.len().min(max_bytes);
        let slice = &bytes[..end];
        match std::str::from_utf8(slice) {
            Ok(text) => Some(text.to_owned()),
            Err(error) => {
                let valid = error.valid_up_to();
                if valid == 0 {
                    None
                } else {
                    std::str::from_utf8(&slice[..valid]).ok().map(str::to_owned)
                }
            }
        }
    }

    fn read_inner_with_deadline(
        &self,
        raw_path: &str,
        options: ReadOptions,
        deadline: &OperationDeadline,
    ) -> Result<ReadResult, FileServiceError> {
        deadline.check()?;
        if options.chunk_bytes == 0
            || options.chunk_bytes > MAX_CHUNK_BYTES
            || options.total_bytes == 0
            || options.total_bytes > MAX_READ_BYTES
        {
            return Err(FileServiceError::InvalidReadOptions);
        }
        self.revalidate_root_with_deadline(deadline)?;
        if !self.lease_guard.is_live() {
            return Err(FileServiceError::AuthorityUnavailable);
        }
        if classify_secret(raw_path) == SecretClassification::SecretLike {
            return Err(FileServiceError::SecretLikePath);
        }
        let path = normalize_relative_path(raw_path)?;
        if classify_secret(path.as_str()) == SecretClassification::SecretLike {
            return Err(FileServiceError::SecretLikePath);
        }
        deadline.check()?;
        let resolved = self.resolve_existing_with_deadline(&path, deadline)?;
        let metadata = resolved.metadata.as_ref().expect("resolved metadata");
        if !metadata.is_file() {
            return Err(FileServiceError::NotRegularFile {
                path: safe_path(&path),
            });
        }
        deadline.check()?;
        let initial = file_fingerprint(resolved.handle.as_ref().expect("resolved file handle"))
            .map_err(|error| self.io_error("stat", path.as_str(), error))?;
        deadline.check()?;
        let initial_parent_identity = resolved.parent_identity;
        let initial_identity = initial.identity;
        drop(resolved);
        deadline.check()?;
        let final_target = self.resolve_existing_with_deadline(&path, deadline)?;
        if final_target.identity != Some(initial_identity)
            || final_target.parent_identity != initial_parent_identity
        {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        let file = final_target.handle.as_ref().expect("final file handle");
        deadline.check()?;
        let before =
            file_fingerprint(file).map_err(|error| self.io_error("stat", path.as_str(), error))?;
        deadline.check()?;
        if before.fingerprint != initial.fingerprint {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        if before.fingerprint.byte_len > options.total_bytes as u64 {
            return Err(FileServiceError::ReadLimitExceeded {
                limit: options.total_bytes,
            });
        }
        let chunk_count = (before.fingerprint.byte_len as usize / options.chunk_bytes)
            + usize::from(before.fingerprint.byte_len as usize % options.chunk_bytes != 0);
        if chunk_count > MAX_CHUNKS_PER_READ {
            return Err(FileServiceError::ChunkLimitExceeded {
                limit: MAX_CHUNKS_PER_READ,
            });
        }
        let mut chunks = Vec::new();
        let mut body = Vec::with_capacity(before.fingerprint.byte_len as usize);
        let mut offset = 0_u64;
        let mut hasher = Sha256::new();
        while offset < before.fingerprint.byte_len {
            deadline.check()?;
            let chunk_len =
                (before.fingerprint.byte_len - offset).min(options.chunk_bytes as u64) as usize;
            let mut bytes = vec![0_u8; chunk_len];
            (&*file)
                .take(chunk_len as u64)
                .read_exact(&mut bytes)
                .map_err(|error| self.io_error("read", path.as_str(), error))?;
            deadline.check()?;
            hasher.update(&bytes);
            body.extend_from_slice(&bytes);
            chunks.push(ReadChunk { offset, bytes });
            offset += chunk_len as u64;
        }
        deadline.check()?;
        let after =
            file_fingerprint(file).map_err(|error| self.io_error("stat", path.as_str(), error))?;
        deadline.check()?;
        if before.fingerprint != after.fingerprint {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        deadline.check()?;
        let latest_target = self.resolve_existing_with_deadline(&path, deadline)?;
        if latest_target.identity != Some(before.identity)
            || latest_target.parent_identity != initial_parent_identity
        {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        let revision = FileRevision {
            fingerprint: after.fingerprint,
            sha256: Some(hasher.finalize().into()),
        };
        self.revalidate_root_with_deadline(deadline)?;
        deadline.check()?;
        Ok(ReadResult {
            path,
            content_kind: classify_content(&body),
            chunks,
            total_bytes: offset,
            revision,
        })
    }

    pub fn read_lines(
        &self,
        raw_path: &str,
        request: LinePageRequest,
    ) -> Result<LinePage, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
            return Err(FileServiceError::PageLimitExceeded {
                limit: MAX_PAGE_SIZE,
            });
        }
        self.revalidate_root_with_deadline(&deadline)?;
        let path = normalize_relative_path(raw_path)?;
        let resolved = self.resolve_existing_with_deadline(&path, &deadline)?;
        let metadata = resolved.metadata.as_ref().expect("resolved metadata");
        if !metadata.is_file() {
            return Err(FileServiceError::NotRegularFile {
                path: safe_path(&path),
            });
        }
        let initial = file_fingerprint(resolved.handle.as_ref().expect("resolved file handle"))
            .map_err(|error| self.io_error("stat", path.as_str(), error))?;
        deadline.check()?;
        let initial_parent_identity = resolved.parent_identity;
        let initial_identity = initial.identity;
        drop(resolved);
        let final_target = self.resolve_existing_with_deadline(&path, &deadline)?;
        if final_target.identity != Some(initial_identity)
            || final_target.parent_identity != initial_parent_identity
        {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        let file = final_target.handle.as_ref().expect("final file handle");
        let before =
            file_fingerprint(file).map_err(|error| self.io_error("stat", path.as_str(), error))?;
        if before.fingerprint != initial.fingerprint {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        let target_len = before.fingerprint.byte_len;
        if let Some(expected) = request.expected_revision.as_ref() {
            if expected.fingerprint != before.fingerprint {
                return Err(FileServiceError::Conflict {
                    path: safe_path(&path),
                });
            }
        }
        if before.fingerprint.byte_len > MAX_READ_BYTES as u64 {
            return Err(FileServiceError::ReadLimitExceeded {
                limit: MAX_READ_BYTES,
            });
        }
        #[cfg(test)]
        test_pause(TEST_PAUSE_BEFORE_LINE_READ);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; MAX_CHUNK_BYTES];
        let mut line_bytes = Vec::new();
        let mut lines = Vec::new();
        let mut line_number = 0_usize;
        let mut offset = 0_u64;
        let mut saw_any = false;
        while offset < target_len {
            deadline.check()?;
            let chunk_len = (target_len - offset).min(MAX_CHUNK_BYTES as u64) as usize;
            let read = (&*file)
                .take(chunk_len as u64)
                .read(&mut buffer)
                .map_err(|error| self.io_error("read lines", path.as_str(), error))?;
            if read == 0 {
                break;
            }
            #[cfg(test)]
            TEST_LINE_READ_BYTES.fetch_add(read, Ordering::AcqRel);
            saw_any = true;
            hasher.update(&buffer[..read]);
            offset += read as u64;
            for byte in &buffer[..read] {
                if *byte == 0 {
                    return Err(FileServiceError::BinaryContent {
                        path: safe_path(&path),
                    });
                }
                if *byte == b'\n' {
                    line_number = line_number.saturating_add(1);
                    if line_number > MAX_LINE_COUNT {
                        return Err(FileServiceError::LineLimitExceeded {
                            limit: MAX_LINE_COUNT,
                        });
                    }
                    let line = line_text(&line_bytes, &path)?;
                    if line_number > request.start_line
                        && line_number <= request.start_line.saturating_add(request.limit)
                    {
                        lines.push(FileLine {
                            number: line_number,
                            text: line,
                        });
                    }
                    line_bytes.clear();
                } else {
                    line_bytes.push(*byte);
                    if line_bytes.len() > MAX_LINE_BYTES {
                        return Err(FileServiceError::LineTooLong {
                            limit: MAX_LINE_BYTES,
                        });
                    }
                }
            }
        }
        if !line_bytes.is_empty() || (saw_any && offset > 0 && line_number == 0) {
            line_number = line_number.saturating_add(1);
            if line_number > MAX_LINE_COUNT {
                return Err(FileServiceError::LineLimitExceeded {
                    limit: MAX_LINE_COUNT,
                });
            }
            let line = line_text(&line_bytes, &path)?;
            if line_number > request.start_line
                && line_number <= request.start_line.saturating_add(request.limit)
            {
                lines.push(FileLine {
                    number: line_number,
                    text: line,
                });
            }
        }
        if request.start_line > line_number {
            return Err(FileServiceError::InvalidPageOffset {
                offset: request.start_line,
            });
        }
        let after =
            file_fingerprint(file).map_err(|error| self.io_error("stat", path.as_str(), error))?;
        let latest_target = self.resolve_existing_with_deadline(&path, &deadline)?;
        if before.fingerprint != after.fingerprint
            || latest_target.identity != Some(before.identity)
            || latest_target.parent_identity != initial_parent_identity
        {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&path),
            });
        }
        let revision = FileRevision {
            fingerprint: after.fingerprint,
            sha256: Some(hasher.finalize().into()),
        };
        if let Some(expected) = request.expected_revision.as_ref() {
            if expected != &revision {
                return Err(FileServiceError::Conflict {
                    path: safe_path(&path),
                });
            }
        }
        self.revalidate_root_with_deadline(&deadline)?;
        deadline.check()?;
        let next = request.start_line.saturating_add(request.limit);
        Ok(LinePage {
            path,
            lines,
            total_lines: line_number,
            next_start_line: (next < line_number).then_some(next),
            revision,
        })
    }

    pub fn search(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<SearchResult, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        self.search_inner(None, query, options, &deadline)
    }

    pub fn search_directory(
        &self,
        relative_directory: Option<&str>,
        query: &str,
        options: SearchOptions,
    ) -> Result<SearchResult, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        self.search_inner(relative_directory, query, options, &deadline)
    }

    fn search_inner(
        &self,
        relative_directory: Option<&str>,
        query: &str,
        options: SearchOptions,
        deadline: &OperationDeadline,
    ) -> Result<SearchResult, FileServiceError> {
        deadline.check()?;
        if query.is_empty()
            || query.len() > MAX_SEARCH_QUERY_BYTES
            || options.max_matches == 0
            || options.max_matches > MAX_SEARCH_MATCHES
            || options.max_files == 0
            || options.max_files > MAX_SEARCH_FILES
            || options.max_bytes == 0
            || options.max_bytes > MAX_SEARCH_BYTES
        {
            return Err(FileServiceError::InvalidSearchOptions);
        }
        let start = relative_directory
            .map(normalize_relative_path)
            .transpose()?;
        let folded_query = (!options.case_sensitive).then(|| query.to_ascii_lowercase());
        let mut pending = vec![(start.clone(), 0_usize)];
        let mut visited = HashSet::new();
        let mut matches = Vec::new();
        let mut scanned_files = 0;
        let mut scanned_bytes = 0;
        let mut visited_entries = 0;
        while let Some((directory, depth)) = pending.pop() {
            deadline.check()?;
            if depth > MAX_SEARCH_DEPTH {
                return Err(FileServiceError::SearchLimitExceeded {
                    limit: MAX_SEARCH_DEPTH,
                });
            }
            let snapshot =
                self.directory_snapshot_path_with_deadline(directory.as_ref(), deadline)?;
            if !visited.insert(snapshot.identity) {
                return Err(FileServiceError::DirectoryChanged {
                    path: directory
                        .as_ref()
                        .map_or_else(|| "<root>".to_string(), safe_path),
                });
            }
            let mut child_directories = Vec::new();
            for entry in snapshot.entries {
                visited_entries += 1;
                deadline.check()?;
                if visited_entries > MAX_SEARCH_ENTRIES {
                    return Err(FileServiceError::SearchLimitExceeded {
                        limit: MAX_SEARCH_ENTRIES,
                    });
                }
                match entry.kind {
                    EntryKind::Directory => {
                        let child_depth = depth.saturating_add(1);
                        if child_depth > MAX_SEARCH_DEPTH {
                            return Err(FileServiceError::SearchLimitExceeded {
                                limit: MAX_SEARCH_DEPTH,
                            });
                        }
                        child_directories.push((entry.path, child_depth));
                    }
                    EntryKind::File => {
                        scanned_files += 1;
                        if scanned_files > options.max_files {
                            return Err(FileServiceError::SearchLimitExceeded {
                                limit: options.max_files,
                            });
                        }
                        let byte_len = entry.byte_len.unwrap_or_default() as usize;
                        let remaining = options.max_bytes.saturating_sub(scanned_bytes);
                        if byte_len > remaining || byte_len > MAX_READ_BYTES {
                            return Err(FileServiceError::SearchLimitExceeded {
                                limit: options.max_bytes,
                            });
                        }
                        let result = self.read_inner_with_deadline(
                            entry.path.as_str(),
                            ReadOptions {
                                chunk_bytes: MAX_CHUNK_BYTES,
                                total_bytes: byte_len.max(1),
                            },
                            deadline,
                        )?;
                        scanned_bytes = scanned_bytes.saturating_add(byte_len);
                        if result.content_kind == ContentKind::Binary {
                            continue;
                        }
                        let body = chunks_to_body(&result.chunks);
                        let text = std::str::from_utf8(&body).map_err(|_| {
                            FileServiceError::BinaryContent {
                                path: safe_path(&result.path),
                            }
                        })?;
                        for (line_index, raw_line) in text.lines().enumerate() {
                            deadline.check()?;
                            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
                            if line.len() > MAX_SEARCH_LINE_BYTES {
                                return Err(FileServiceError::SearchLimitExceeded {
                                    limit: MAX_SEARCH_LINE_BYTES,
                                });
                            }
                            let haystack = folded_query
                                .as_ref()
                                .map_or_else(|| line.to_string(), |_| line.to_ascii_lowercase());
                            let needle = folded_query.as_deref().unwrap_or(query);
                            let Some(byte_column) = haystack.find(needle) else {
                                continue;
                            };
                            if matches.len() >= options.max_matches {
                                return Err(FileServiceError::SearchLimitExceeded {
                                    limit: options.max_matches,
                                });
                            }
                            matches.push(SearchMatch {
                                path: result.path.clone(),
                                line: line_index + 1,
                                column: haystack[..byte_column].chars().count() + 1,
                                text: line.to_string(),
                            });
                        }
                    }
                    EntryKind::Other => {}
                }
            }
            for (directory, depth) in child_directories.into_iter().rev() {
                pending.push((Some(directory), depth));
            }
        }
        self.revalidate_root_with_deadline(deadline)?;
        deadline.check()?;
        Ok(SearchResult {
            matches,
            scanned_files,
            scanned_bytes,
        })
    }

    pub fn current_revision(&self, raw_path: &str) -> Result<FileRevision, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        Ok(self
            .read_inner_with_deadline(raw_path, ReadOptions::default(), &deadline)?
            .revision)
    }

    pub fn plan_write(
        &self,
        raw_path: &str,
        contents: Vec<u8>,
        expected: ExpectedRevision,
    ) -> Result<WritePlan, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        if contents.len() > MAX_READ_BYTES {
            return Err(FileServiceError::WriteLimitExceeded {
                limit: MAX_READ_BYTES,
            });
        }
        self.revalidate_root_with_deadline(&deadline)?;
        let path = normalize_relative_path(raw_path)?;
        deadline.check()?;
        let resolved = self.resolve_target_with_deadline(&path, true, &deadline)?;
        deadline.check()?;
        let actual =
            self.current_expected_state_with_deadline(&path, &expected, &resolved, &deadline)?;
        ensure_expected(&expected, actual.as_ref(), &path)?;
        Ok(WritePlan {
            record: MutationRecord {
                service_authority: self.authority,
                authority: self.binding,
                path,
                expected,
                commit_revision: actual.clone(),
                target_identity: resolved.identity,
                parent_identity: resolved.parent_identity,
            },
            secret: classify_secret(raw_path),
            contents,
        })
    }

    pub fn execute_write(&self, plan: WritePlan) -> Result<WriteResult, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        let lock = self.target_lock(plan.record.path.as_str(), &deadline)?;
        let _guard = lock.lock_until(&deadline)?;
        deadline.check()?;
        if plan.record.service_authority != self.authority || plan.record.authority != self.binding
        {
            return Err(FileServiceError::ForeignPlan);
        }
        self.revalidate_root_with_deadline(&deadline)?;
        ensure_mutation_capability("write")?;
        self.recover_tombstones(&deadline)?;
        let mut _tombstone_reservation = self.reserve_tombstone_slot()?;
        deadline.check()?;
        let resolved = self.resolve_target_with_deadline(&plan.record.path, true, &deadline)?;
        self.validate_record_target(&plan.record, &resolved)?;
        deadline.check()?;
        let actual = self.current_expected_state_with_deadline(
            &plan.record.path,
            &plan.record.expected,
            &resolved,
            &deadline,
        )?;
        ensure_expected(&plan.record.expected, actual.as_ref(), &plan.record.path)?;
        let original_permission_bits = resolved.metadata.as_ref().map(permission_bits);
        let parent = resolved
            .parent_write_handle
            .as_ref()
            .ok_or_else(|| FileServiceError::Io {
                operation: "open writable parent",
                path: safe_path(&plan.record.path),
                kind: io::ErrorKind::PermissionDenied,
                raw_code: None,
            })?;
        deadline.check()?;
        let cleanup_parent = parent.try_clone().map_err(|error| {
            self.io_error("clone temporary parent", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        let mut temporary = create_sibling_temp(
            parent,
            &resolved.parent_path,
            Arc::clone(&self.cleanup),
            resolved.parent_identity,
            resolved.identity,
            &deadline,
        )
        .map_err(|error| {
            self.deadline_aware_io_error("create temporary file", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        let cleanup_temporary = temporary.file.try_clone().map_err(|error| {
            self.io_error("clone temporary file", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        let temporary_identity = temporary.identity;
        let mut cleanup = TempCleanup::from_temporary(
            &mut temporary,
            cleanup_parent,
            cleanup_temporary,
            Arc::clone(&self.cleanup),
            resolved.parent_identity,
            deadline.clone(),
        );
        deadline.check()?;
        let post_create = self.resolve_target_with_deadline(&plan.record.path, true, &deadline)?;
        self.validate_record_target(&plan.record, &post_create)?;
        drop(post_create);
        deadline.check()?;
        preserve_permissions(
            resolved.metadata.as_ref().zip(resolved.handle.as_ref()),
            &temporary.file,
            &plan.record.path,
            &deadline,
        )?;
        deadline.check()?;
        temporary.file.write_all(&plan.contents).map_err(|error| {
            self.io_error("write temporary file", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        temporary.file.flush().map_err(|error| {
            self.io_error("flush temporary file", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        temporary.file.sync_all().map_err(|error| {
            self.io_error("sync temporary file", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        drop(resolved);
        self.revalidate_root_with_deadline(&deadline)?;
        let final_check = self.resolve_target_with_deadline(&plan.record.path, true, &deadline)?;
        self.validate_record_target(&plan.record, &final_check)?;
        deadline.check()?;
        let final_actual = self.current_expected_state_with_deadline(
            &plan.record.path,
            &plan.record.expected,
            &final_check,
            &deadline,
        )?;
        ensure_expected(
            &plan.record.expected,
            final_actual.as_ref(),
            &plan.record.path,
        )?;
        if final_actual.as_ref() != plan.record.commit_revision.as_ref() {
            return Err(FileServiceError::Conflict {
                path: safe_path(&plan.record.path),
            });
        }
        let replacing = final_check.identity.is_some();
        let final_parent_source =
            final_check
                .parent_write_handle
                .as_ref()
                .ok_or_else(|| FileServiceError::Io {
                    operation: "open writable parent",
                    path: safe_path(&plan.record.path),
                    kind: io::ErrorKind::PermissionDenied,
                    raw_code: None,
                })?;
        deadline.check()?;
        let final_parent = final_parent_source.try_clone().map_err(|error| {
            self.io_error("clone parent handle", plan.record.path.as_str(), error)
        })?;
        let final_parent_path = final_check.parent_path.clone();
        let final_parent_identity = final_check.parent_identity;
        let final_destination_name = final_check.name.clone();
        drop(final_check);
        #[cfg(test)]
        test_pause(TEST_PAUSE_BEFORE_RENAME);
        deadline.check()?;
        self.revalidate_parent_within_root(&final_parent_path, final_parent_identity, &deadline)?;
        let replacement = atomic_replace(
            &final_parent,
            &temporary.file,
            &temporary.name,
            final_destination_name.as_str(),
            replacing,
            plan.record.commit_revision.as_ref(),
            Sha256::digest(&plan.contents).into(),
            &self.cleanup,
            &deadline,
        );
        if deadline.check().is_err() {
            match &replacement {
                Ok(()) => {
                    cleanup.disarm();
                }
                Err(AtomicReplaceError::Tombstone {
                    name,
                    identity,
                    temporary_moved,
                    destination_committed,
                }) => {
                    let _ = self.retain_cleanup_after_effect(
                        &final_parent,
                        final_parent_identity,
                        name.clone(),
                        *identity,
                        &mut _tombstone_reservation,
                    );
                    if *temporary_moved || *destination_committed {
                        cleanup.disarm();
                    }
                }
                Err(AtomicReplaceError::Uncertain {
                    name: Some(name),
                    identity: Some(identity),
                    ..
                }) => {
                    let _ = self.retain_cleanup_after_effect(
                        &final_parent,
                        final_parent_identity,
                        name.clone(),
                        *identity,
                        &mut _tombstone_reservation,
                    );
                    cleanup.disarm();
                }
                Err(error) => {
                    if error.temporary_moved() || error.destination_committed() {
                        cleanup.disarm();
                    }
                }
            }
            return Err(FileServiceError::DeadlineExceeded);
        }
        match replacement {
            Ok(()) => {
                cleanup.disarm();
            }
            Err(error) => {
                let temporary_moved = error.temporary_moved();
                if temporary_moved || error.destination_committed() {
                    cleanup.disarm();
                }
                match error {
                    AtomicReplaceError::Conflict { .. } => {
                        return Err(FileServiceError::Conflict {
                            path: safe_path(&plan.record.path),
                        })
                    }
                    AtomicReplaceError::Uncertain {
                        name: Some(name),
                        identity: Some(identity),
                        ..
                    } => {
                        cleanup.disarm();
                        self.retain_cleanup_after_effect(
                            &final_parent,
                            final_parent_identity,
                            name,
                            identity,
                            &mut _tombstone_reservation,
                        )?;
                        return Err(FileServiceError::CleanupFailed);
                    }
                    AtomicReplaceError::Uncertain { .. } => {
                        // The destination may already be committed, but no
                        // exact old-inode binding survived the post-commit
                        // race. Keep all residue private and make the
                        // uncertainty visible instead of registering a name
                        // that could later identify an attacker inode.
                        let uncertain = self.record_uncertain_cleanup(
                            &mut _tombstone_reservation,
                            &cleanup.parent,
                            cleanup.parent_identity,
                            cleanup.name.as_str(),
                            temporary_identity,
                            &deadline,
                        );
                        if uncertain.is_ok() {
                            cleanup.disarm();
                        }
                        uncertain?;
                        return Err(FileServiceError::CleanupFailed);
                    }
                    AtomicReplaceError::Io { error, .. } => {
                        return Err(self.io_error(
                            "replace file atomically",
                            plan.record.path.as_str(),
                            error,
                        ))
                    }
                    AtomicReplaceError::Tombstone {
                        name,
                        identity,
                        temporary_moved,
                        destination_committed,
                    } => {
                        // If the temporary inode was not moved, leave its
                        // exact cleanup binding armed while the old inode is
                        // retained. This covers install failure after the
                        // old destination was detached and a replacement
                        // destination appeared concurrently.
                        if temporary_moved || destination_committed {
                            cleanup.disarm();
                        }
                        self.retain_tombstone(
                            &final_parent,
                            final_parent_identity,
                            name,
                            identity,
                            &mut _tombstone_reservation,
                            &deadline,
                        )?;
                        return Err(FileServiceError::Conflict {
                            path: safe_path(&plan.record.path),
                        });
                    }
                }
            }
        }
        cleanup.disarm();
        drop(cleanup);
        deadline.check()?;
        sync_parent_directory_with_deadline(&final_parent, &deadline).map_err(|error| {
            self.deadline_aware_io_error("flush parent directory", plan.record.path.as_str(), error)
        })?;
        deadline.check()?;
        let written = self.read_inner_with_deadline(
            plan.record.path.as_str(),
            ReadOptions::default(),
            &deadline,
        )?;
        let expected_hash: [u8; 32] = Sha256::digest(&plan.contents).into();
        if written.revision.sha256 != Some(expected_hash)
            || written.total_bytes != plan.contents.len() as u64
        {
            return Err(FileServiceError::ChangedDuringRead {
                path: safe_path(&plan.record.path),
            });
        }
        if original_permission_bits
            .is_some_and(|bits| written.revision.fingerprint.permission_bits != bits)
        {
            return Err(FileServiceError::PermissionPreservationFailed {
                path: safe_path(&plan.record.path),
            });
        }
        Ok(WriteResult {
            path: plan.record.path,
            bytes_written: plan.contents.len(),
            revision: written.revision,
        })
    }

    pub fn plan_delete(
        &self,
        raw_path: &str,
        expected: ExpectedRevision,
    ) -> Result<DeletePreview, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        self.revalidate_root_with_deadline(&deadline)?;
        let path = normalize_relative_path(raw_path)?;
        deadline.check()?;
        let resolved = self.resolve_existing_with_deadline(&path, &deadline)?;
        if !resolved
            .metadata
            .as_ref()
            .is_some_and(fs::Metadata::is_file)
        {
            return Err(FileServiceError::NotRegularFile {
                path: safe_path(&path),
            });
        }
        deadline.check()?;
        let revision =
            self.revision_for_expected_with_deadline(&path, &expected, &resolved, &deadline)?;
        ensure_expected(&expected, Some(&revision), &path)?;
        Ok(DeletePreview {
            record: MutationRecord {
                service_authority: self.authority,
                authority: self.binding,
                path,
                expected,
                commit_revision: Some(revision.clone()),
                target_identity: resolved.identity,
                parent_identity: resolved.parent_identity,
            },
            revision,
            secret: classify_secret(raw_path),
        })
    }

    pub fn execute_delete(&self, preview: DeletePreview) -> Result<DeleteResult, FileServiceError> {
        let _permit = self.try_acquire_operation()?;
        let deadline = self.operation_deadline();
        deadline.check()?;
        let lock = self.target_lock(preview.record.path.as_str(), &deadline)?;
        let _guard = lock.lock_until(&deadline)?;
        deadline.check()?;
        if preview.record.service_authority != self.authority
            || preview.record.authority != self.binding
        {
            return Err(FileServiceError::ForeignPlan);
        }
        self.revalidate_root_with_deadline(&deadline)?;
        ensure_mutation_capability("delete")?;
        self.recover_tombstones(&deadline)?;
        let mut _tombstone_reservation = self.reserve_tombstone_slot()?;
        deadline.check()?;
        let mut resolved = self.resolve_existing_with_deadline(&preview.record.path, &deadline)?;
        self.validate_record_target(&preview.record, &resolved)?;
        deadline.check()?;
        let current = self.revision_for_expected_with_deadline(
            &preview.record.path,
            &preview.record.expected,
            &resolved,
            &deadline,
        )?;
        if current != preview.revision {
            return Err(FileServiceError::Conflict {
                path: safe_path(&preview.record.path),
            });
        }
        #[cfg(test)]
        test_pause(TEST_PAUSE_BEFORE_DELETE_EFFECT);
        self.revalidate_parent_within_root(
            &resolved.parent_path,
            resolved.parent_identity,
            &deadline,
        )?;
        #[cfg(windows)]
        {
            // The validation handle is deliberately closed before acquiring the
            // delete-capable handle. The second open revalidates the exact file
            // identity and avoids depending on another handle's share mode.
            drop(resolved.handle.take());
            let delete_parent =
                resolved
                    .parent_write_handle
                    .as_ref()
                    .ok_or_else(|| FileServiceError::Io {
                        operation: "open writable parent",
                        path: safe_path(&preview.record.path),
                        kind: io::ErrorKind::PermissionDenied,
                        raw_code: None,
                    })?;
            let delete_handle = {
                deadline.check()?;
                open_child_nofollow_for_delete(delete_parent, resolved.name.as_str())
            }
            .map_err(|error| {
                self.io_error("open file for delete", preview.record.path.as_str(), error)
            })?;
            deadline.check()?;
            let (delete_identity, link_count) =
                opened_file_info(&delete_handle).map_err(|error| {
                    self.io_error("stat file for delete", preview.record.path.as_str(), error)
                })?;
            if link_count != 1 || Some(delete_identity) != resolved.identity {
                return Err(if link_count != 1 {
                    FileServiceError::HardLinkRejected {
                        path: safe_path(&preview.record.path),
                    }
                } else {
                    FileServiceError::Conflict {
                        path: safe_path(&preview.record.path),
                    }
                });
            }
            deadline.check()?;
            let delete_revision =
                revision_from_opened_file_with_deadline(&delete_handle, &deadline).map_err(
                    |error| {
                        self.deadline_aware_io_error(
                            "hash file for delete",
                            preview.record.path.as_str(),
                            error,
                        )
                    },
                )?;
            if delete_revision != preview.revision {
                return Err(FileServiceError::Conflict {
                    path: safe_path(&preview.record.path),
                });
            }
            deadline.check()?;
            delete_opened_file(&delete_handle).map_err(|error| {
                self.io_error("delete file", preview.record.path.as_str(), error)
            })?;
            if deadline.check().is_err() {
                self.record_uncertain_cleanup(
                    &mut _tombstone_reservation,
                    delete_parent,
                    resolved.parent_identity,
                    resolved.name.as_str(),
                    delete_identity,
                    &deadline,
                )?;
                return Err(FileServiceError::DeadlineExceeded);
            }
        }
        #[cfg(unix)]
        {
            deadline.check()?;
            let delete_parent =
                resolved
                    .parent_write_handle
                    .as_ref()
                    .ok_or_else(|| FileServiceError::Io {
                        operation: "open writable parent",
                        path: safe_path(&preview.record.path),
                        kind: io::ErrorKind::PermissionDenied,
                        raw_code: None,
                    })?;
            let delete_identity = resolved.identity.expect("resolved target identity");
            let delete_result = delete_unix_if_identity(
                delete_parent,
                resolved.name.as_str(),
                delete_identity,
                &current,
                &self.cleanup,
                &deadline,
            );
            if deadline.check().is_err() {
                match delete_result {
                    Err(AtomicReplaceError::Tombstone {
                        name: generated_tombstone,
                        identity,
                        ..
                    })
                    | Err(AtomicReplaceError::Uncertain {
                        name: Some(generated_tombstone),
                        identity: Some(identity),
                        ..
                    }) => {
                        // The detach already committed, so the operation
                        // budget is no longer usable for another blocking
                        // observation. Record the exact generated residue
                        // without rechecking the expired deadline; a public
                        // pathname is never an acceptable fallback.
                        let _ = self.retain_cleanup_after_effect(
                            delete_parent,
                            resolved.parent_identity,
                            generated_tombstone,
                            identity,
                            &mut _tombstone_reservation,
                        );
                    }
                    _ => {}
                }
                return Err(FileServiceError::DeadlineExceeded);
            }
            match delete_result {
                Ok(()) => {}
                Err(AtomicReplaceError::Conflict { .. }) => {
                    return Err(FileServiceError::Conflict {
                        path: safe_path(&preview.record.path),
                    })
                }
                Err(AtomicReplaceError::Uncertain {
                    name: Some(generated_tombstone),
                    identity: Some(identity),
                    ..
                }) => {
                    self.retain_tombstone(
                        delete_parent,
                        resolved.parent_identity,
                        generated_tombstone,
                        identity,
                        &mut _tombstone_reservation,
                        &deadline,
                    )?;
                    return Err(FileServiceError::CleanupFailed);
                }
                Err(AtomicReplaceError::Uncertain { .. }) => {
                    return Err(FileServiceError::CleanupFailed);
                }
                Err(AtomicReplaceError::Io { error, .. }) => {
                    return Err(self.io_error("delete file", preview.record.path.as_str(), error))
                }
                Err(AtomicReplaceError::Tombstone { name, identity, .. }) => {
                    let parent = resolved
                        .parent_write_handle
                        .as_ref()
                        .ok_or_else(|| FileServiceError::RootUnavailable)?;
                    self.retain_tombstone(
                        parent,
                        resolved.parent_identity,
                        name,
                        identity,
                        &mut _tombstone_reservation,
                        &deadline,
                    )?;
                    return Err(FileServiceError::Conflict {
                        path: safe_path(&preview.record.path),
                    });
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            deadline.check()?;
            fs::remove_file(&resolved.full_path).map_err(|error| {
                self.io_error("delete file", preview.record.path.as_str(), error)
            })?;
            if deadline.check().is_err() {
                return Err(FileServiceError::DeadlineExceeded);
            }
        }
        #[cfg(any(unix, windows))]
        {
            let parent =
                resolved
                    .parent_write_handle
                    .as_ref()
                    .ok_or_else(|| FileServiceError::Io {
                        operation: "open writable parent",
                        path: safe_path(&preview.record.path),
                        kind: io::ErrorKind::PermissionDenied,
                        raw_code: None,
                    })?;
            deadline.check()?;
            sync_parent_directory_with_deadline(parent, &deadline).map_err(|error| {
                self.deadline_aware_io_error(
                    "flush parent directory",
                    preview.record.path.as_str(),
                    error,
                )
            })?;
        }
        self.revalidate_root_with_deadline(&deadline)?;
        deadline.check()?;
        if self
            .resolve_target_with_deadline(&preview.record.path, true, &deadline)?
            .identity
            .is_some()
        {
            return Err(FileServiceError::Conflict {
                path: safe_path(&preview.record.path),
            });
        }
        Ok(DeleteResult {
            path: preview.record.path,
            revision: preview.revision,
        })
    }

    fn target_lock(
        &self,
        path: &str,
        deadline: &OperationDeadline,
    ) -> Result<Arc<DeadlineMutex<()>>, FileServiceError> {
        deadline.check()?;
        let mut locks = self.mutation_locks.lock_until(deadline)?;
        deadline.check()?;
        let key = path_key_text(path);
        if !locks.contains_key(&key) && locks.len() >= MAX_MUTATION_LOCKS {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            if locks.len() >= MAX_MUTATION_LOCKS {
                return Err(FileServiceError::ConcurrencyLimitExceeded {
                    limit: MAX_MUTATION_LOCKS,
                });
            }
        }
        Ok(Arc::clone(
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(DeadlineMutex::new(()))),
        ))
    }

    fn reserve_tombstone_slot(&self) -> Result<TombstoneReservation, FileServiceError> {
        if self.cleanup.try_reserve() {
            Ok(TombstoneReservation {
                ledger: Arc::clone(&self.cleanup),
                released: false,
            })
        } else {
            Err(FileServiceError::ConcurrencyLimitExceeded {
                limit: MAX_TOMBSTONES,
            })
        }
    }

    fn record_uncertain_cleanup(
        &self,
        reservation: &mut TombstoneReservation,
        parent: &File,
        parent_identity: FileIdentity,
        name: &str,
        identity: FileIdentity,
        deadline: &OperationDeadline,
    ) -> Result<(), FileServiceError> {
        if reservation.released {
            return Err(FileServiceError::ConcurrencyLimitExceeded {
                limit: MAX_TOMBSTONES,
            });
        }
        if !is_private_cleanup_name(name) {
            // A guard that lost its private generated name is foreign
            // uncertainty. Recording a public pathname would let a later
            // restart adopt a same-name replacement.
            return Err(FileServiceError::CleanupFailed);
        }
        deadline.check()?;
        let parent = parent
            .try_clone()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        deadline.check()?;
        let ledger = Arc::clone(&reservation.ledger);
        insert_reserved_cleanup_record(
            &ledger,
            reservation,
            TombstoneRecord {
                parent,
                parent_identity,
                expected_parent_identity: parent_identity,
                name: name.to_string(),
                identity,
                expected_target_identity: cleanup_expected_target(name),
                operation_nonce: cleanup_operation_nonce(name, identity),
                uncertain: true,
                recovering: false,
            },
            deadline,
        )?;
        Ok(())
    }

    fn retain_tombstone(
        &self,
        parent: &File,
        parent_identity: FileIdentity,
        name: String,
        identity: FileIdentity,
        reservation: &mut TombstoneReservation,
        deadline: &OperationDeadline,
    ) -> Result<(), FileServiceError> {
        deadline.check()?;
        let binding = parse_tombstone_binding(&name);
        if binding.is_none()
            || binding.is_some_and(|binding| {
                binding.identity != identity
                    || binding
                        .parent_identity
                        .is_some_and(|expected| expected != parent_identity)
            })
        {
            // A durable record is valid only for a generated tombstone name
            // carrying the identity that was proven before detachment. Never
            // queue a normal temporary/destination pathname: a later startup
            // could otherwise delete a concurrently substituted inode.
            self.record_uncertain_cleanup(
                reservation,
                parent,
                parent_identity,
                name.as_str(),
                identity,
                deadline,
            )?;
            return Err(FileServiceError::CleanupFailed);
        }
        // Validate the name again before recording it. A post-commit cleanup
        // helper may have moved the exact inode to a held authority and then
        // failed; retaining a record for the now-absent name would not be a
        // durable identity binding. Conversely, if a writer substituted the
        // name, the mismatch must become visible uncertainty rather than a
        // record that startup could later act on.
        #[cfg(any(unix, windows))]
        {
            deadline.check()?;
            let observed = open_child_nofollow(parent, &name)
                .and_then(|file| opened_file_info(&file))
                .ok();
            // The descriptor-bound AT_EMPTY_PATH fallback may leave the exact
            // inode with more than one link. The generated name remains safe
            // to retain/recover when its identity matches; only zero links
            // means the name was no longer observed.
            if !observed.is_some_and(|(observed_identity, links)| {
                observed_identity == identity && links > 0
            }) {
                self.record_uncertain_cleanup(
                    reservation,
                    parent,
                    parent_identity,
                    name.as_str(),
                    identity,
                    deadline,
                )?;
                return Err(FileServiceError::CleanupFailed);
            }
        }
        deadline.check()?;
        let parent = match parent.try_clone() {
            Ok(parent) => parent,
            Err(_) => {
                // The generated name is still present, but the service could
                // not retain its parent authority. Count the post-commit
                // residue as uncertainty rather than dropping the reservation
                // and allowing it to escape the live cap.
                self.record_uncertain_cleanup(
                    reservation,
                    parent,
                    parent_identity,
                    name.as_str(),
                    identity,
                    deadline,
                )?;
                return Err(FileServiceError::RootUnavailable);
            }
        };
        deadline.check()?;
        let operation_nonce = cleanup_operation_nonce(name.as_str(), identity);
        insert_reserved_cleanup_record(
            &self.cleanup,
            reservation,
            TombstoneRecord {
                parent,
                parent_identity,
                expected_parent_identity: parent_identity,
                name,
                identity,
                expected_target_identity: Some(identity),
                operation_nonce,
                uncertain: false,
                recovering: false,
            },
            deadline,
        )?;
        Ok(())
    }

    fn retain_cleanup_after_effect(
        &self,
        parent: &File,
        parent_identity: FileIdentity,
        name: String,
        identity: FileIdentity,
        reservation: &mut TombstoneReservation,
    ) -> Result<(), FileServiceError> {
        let Some(binding) = cleanup_name_binding(&name) else {
            return Err(FileServiceError::CleanupFailed);
        };
        if !cleanup_binding_matches_identity(&name, binding, identity)
            || binding.parent_identity != Some(parent_identity)
            || (!binding.expected_target_identity.is_some_and(|target| {
                target == identity
                    || (identity_is_zero(target) && parse_temporary_binding(&name).is_some())
            }))
        {
            return Err(FileServiceError::CleanupFailed);
        }
        let parent = parent
            .try_clone()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        let record = TombstoneRecord {
            parent,
            parent_identity,
            expected_parent_identity: parent_identity,
            name,
            identity,
            expected_target_identity: binding.expected_target_identity,
            operation_nonce: binding.operation_nonce,
            uncertain: true,
            recovering: false,
        };
        insert_reserved_cleanup_record_nonblocking(&self.cleanup, reservation, record);
        Ok(())
    }

    /// Recover only tombstones whose private name carries the inode identity
    /// that is still present at that name. The scan is descriptor-relative and
    /// bounded; a reused name or an unrelated entry is left untouched.
    #[cfg(any(unix, windows))]
    fn discover_tombstones(&self, deadline: &OperationDeadline) -> Result<(), FileServiceError> {
        #[cfg(target_os = "macos")]
        {
            // Darwin currently supports safe descriptor-relative reads/listing
            // through `/dev/fd`, but no handle/name atomic mutation primitive
            // is implemented here. Construction must therefore remain usable
            // for reads without attempting recovery that would imply write
            // capability; mutations fail closed as Unsupported.
            deadline.check()?;
            return Ok(());
        }
        deadline.check()?;
        let root = self
            .root
            .handle
            .try_clone()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        let mut pending = vec![(root, self.root.path.clone(), 0_usize)];
        let mut scanned_directories = 0_usize;
        while let Some((directory, fallback, depth)) = pending.pop() {
            deadline.check()?;
            scanned_directories = scanned_directories.saturating_add(1);
            if scanned_directories > MAX_SEARCH_ENTRIES {
                return Err(FileServiceError::ConcurrencyLimitExceeded {
                    limit: MAX_SEARCH_ENTRIES,
                });
            }
            deadline.check()?;
            let parent_identity = opened_file_info(&directory)
                .map_err(|_| FileServiceError::RootUnavailable)?
                .0;
            deadline.check()?;
            let entries = read_directory_from_handle(&directory, &fallback)
                .map_err(|_| FileServiceError::RootUnavailable)?;
            deadline.check()?;
            for entry in entries {
                deadline.check()?;
                let Some(name) = entry.to_str() else {
                    continue;
                };
                deadline.check()?;
                let child = match open_child_nofollow(&directory, name) {
                    Ok(child) => child,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => continue,
                };
                deadline.check()?;
                let metadata = match child.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                deadline.check()?;
                if metadata.is_dir() {
                    if depth < MAX_SEARCH_DEPTH {
                        pending.push((child, fallback.join(name), depth.saturating_add(1)));
                    }
                    continue;
                }
                let tombstone_binding = parse_tombstone_binding(name);
                let temporary_binding = parse_temporary_binding(name);
                if tombstone_binding.is_none() && temporary_binding.is_none() {
                    continue;
                }
                let binding = tombstone_binding
                    .or(temporary_binding)
                    .expect("binding exists");
                // A generated name is recoverable only when its parent and
                // operation target are explicitly bound. Missing-target
                // temporary intents and malformed names are visible uncertainty,
                // never adoption candidates.
                if binding.expected_target_identity.is_none()
                    || binding.parent_identity != Some(parent_identity)
                {
                    continue;
                }
                deadline.check()?;
                let (identity, link_count) = match opened_file_info(&child) {
                    Ok(info) => info,
                    Err(_) => continue,
                };
                deadline.check()?;
                // A descriptor-bound recovery link can legitimately have a
                // positive link count greater than one. Identity, not the
                // count, authorizes removal of this generated name.
                if link_count == 0 || !cleanup_binding_matches_identity(name, binding, identity) {
                    continue;
                }
                let Some(mut reservation) = reserve_cleanup_slot(&self.cleanup) else {
                    return Err(FileServiceError::ConcurrencyLimitExceeded {
                        limit: MAX_TOMBSTONES,
                    });
                };
                deadline.check()?;
                let parent = match directory.try_clone() {
                    Ok(parent) => parent,
                    Err(_) => {
                        reservation.release();
                        return Err(FileServiceError::RootUnavailable);
                    }
                };
                let record = TombstoneRecord {
                    parent,
                    parent_identity,
                    expected_parent_identity: parent_identity,
                    name: name.to_string(),
                    identity,
                    expected_target_identity: binding.expected_target_identity,
                    operation_nonce: binding.operation_nonce,
                    uncertain: false,
                    recovering: false,
                };
                insert_reserved_cleanup_record(&self.cleanup, &mut reservation, record, deadline)?;
            }
        }
        self.recover_tombstones(deadline)
    }

    #[cfg(not(any(unix, windows)))]
    fn discover_tombstones(&self, deadline: &OperationDeadline) -> Result<(), FileServiceError> {
        deadline.check()?;
        Ok(())
    }

    fn reset_recovery_marks(&self, deadline: &OperationDeadline) -> Result<(), FileServiceError> {
        deadline.check()?;
        let mut records = self.cleanup.tombstones.lock_until(deadline)?;
        deadline.check()?;
        for record in records.iter_mut() {
            record.recovering = false;
        }
        deadline.check()?;
        Ok(())
    }

    fn next_recovery_record(
        &self,
        deadline: &OperationDeadline,
    ) -> Result<Option<TombstoneRecord>, FileServiceError> {
        deadline.check()?;
        let mut records = self.cleanup.tombstones.lock_until(deadline)?;
        deadline.check()?;
        for record in records.iter_mut() {
            if record.recovering {
                continue;
            }
            deadline.check()?;
            let parent = record
                .parent
                .try_clone()
                .map_err(|_| FileServiceError::RootUnavailable)?;
            deadline.check()?;
            record.recovering = true;
            return Ok(Some(TombstoneRecord {
                parent,
                parent_identity: record.parent_identity,
                expected_parent_identity: record.expected_parent_identity,
                name: record.name.clone(),
                identity: record.identity,
                expected_target_identity: record.expected_target_identity,
                operation_nonce: record.operation_nonce,
                uncertain: record.uncertain,
                recovering: true,
            }));
        }
        Ok(None)
    }

    fn finish_recovery_record(
        &self,
        record: &TombstoneRecord,
        keep: bool,
        deadline: &OperationDeadline,
    ) -> Result<(), FileServiceError> {
        deadline.check()?;
        let mut records = self.cleanup.tombstones.lock_until(deadline)?;
        deadline.check()?;
        let Some(index) = records.iter().position(|candidate| {
            candidate.recovering
                && ((candidate.parent_identity == record.parent_identity
                    && candidate.name == record.name
                    && candidate.identity == record.identity)
                    // A Linux authority transfer updates the current path in
                    // place but preserves the opaque operation nonce so the
                    // original recovery slot can still be settled.
                    || candidate.operation_nonce == record.operation_nonce)
        }) else {
            return Ok(());
        };
        if keep {
            records[index].recovering = false;
        } else {
            let removed = records.swap_remove(index);
            if removed.uncertain {
                self.cleanup
                    .uncertain_cleanups
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_sub(1))
                    })
                    .ok();
            }
            self.cleanup.release();
        }
        deadline.check()?;
        Ok(())
    }

    fn recover_record(
        &self,
        record: &TombstoneRecord,
        deadline: &OperationDeadline,
    ) -> Result<bool, FileServiceError> {
        deadline.check()?;
        let Some(binding) = cleanup_name_binding(&record.name) else {
            return Ok(true);
        };
        if !cleanup_binding_matches_identity(&record.name, binding, record.identity)
            || binding.parent_identity != Some(record.expected_parent_identity)
            || binding.expected_target_identity != record.expected_target_identity
            || binding.operation_nonce != record.operation_nonce
        {
            // A same-slot record whose durable name no longer carries the
            // exact parent/target/nonce binding is visible uncertainty, never
            // a candidate for deletion.
            return Ok(true);
        }
        let (parent_identity, parent_links) =
            opened_file_info(&record.parent).map_err(|_| FileServiceError::RootUnavailable)?;
        deadline.check()?;
        if parent_identity != record.parent_identity || parent_links == 0 {
            return Ok(true);
        }
        #[cfg(unix)]
        {
            #[cfg(target_os = "linux")]
            if unlink_exchange_temporary_aliases(
                &record.parent,
                record.identity,
                record.expected_parent_identity,
                deadline,
            )
            .is_err()
            {
                // A failed alias unlink leaves the identity-bound anchor in
                // place. Keep this record so the next bounded recovery pass
                // can retry the exact temporary link.
                return Ok(true);
            }
            let observed = open_child_nofollow(&record.parent, &record.name)
                .and_then(|file| opened_file_info(&file));
            deadline.check()?;
            match observed {
                Ok((identity, link_count))
                    if identity == record.identity
                        && link_count > 0
                        && is_private_cleanup_name(&record.name) =>
                {
                    deadline.check()?;
                    let unlink_result = unlink_private_name_if_identity_with_slot(
                        &record.parent,
                        &record.name,
                        record.identity,
                        &self.cleanup,
                        true,
                        deadline,
                    );
                    deadline.check()?;
                    if unlink_result.is_ok() {
                        let sync_result =
                            sync_parent_directory_with_deadline(&record.parent, deadline);
                        deadline.check()?;
                        if sync_result.is_ok() {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(_) => return Ok(true),
            }
        }
        #[cfg(windows)]
        {
            match open_child_nofollow_for_cleanup(&record.parent, &record.name) {
                Ok(file) => {
                    let observed = opened_file_info(&file);
                    deadline.check()?;
                    match observed {
                        Ok((identity, link_count))
                            if identity == record.identity
                                && link_count > 0
                                && is_private_cleanup_name(&record.name) =>
                        {
                            deadline.check()?;
                            let delete_result = delete_opened_file(&file);
                            deadline.check()?;
                            if delete_result.is_ok() {
                                let sync_result =
                                    sync_parent_directory_with_deadline(&record.parent, deadline);
                                deadline.check()?;
                                if sync_result.is_ok() {
                                    return Ok(false);
                                }
                            }
                            return Ok(true);
                        }
                        Ok(_) => return Ok(true),
                        Err(_) => return Ok(true),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(_) => return Ok(true),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (self, record);
            deadline.check()?;
            Ok(true)
        }
    }

    fn recover_tombstones(&self, deadline: &OperationDeadline) -> Result<(), FileServiceError> {
        self.reset_recovery_marks(deadline)?;
        loop {
            let Some(record) = self.next_recovery_record(deadline)? else {
                return Ok(());
            };
            let keep = self.recover_record(&record, deadline)?;
            self.finish_recovery_record(&record, keep, deadline)?;
        }
    }

    #[cfg(test)]
    pub(crate) fn cache_sizes_for_test(&self) -> (usize, usize) {
        let mutation_locks = self.mutation_locks.try_lock().expect("mutation lock cache");
        let deadline = OperationDeadline::new();
        let directory_identities = self
            .directory_identities
            .lock_until(&deadline)
            .expect("directory identity cache");
        (mutation_locks.len(), directory_identities.len())
    }

    #[cfg(test)]
    pub(crate) fn churn_caches_for_test(
        &self,
        count: usize,
    ) -> Result<(usize, usize), FileServiceError> {
        for index in 0..count {
            let deadline = OperationDeadline::new();
            self.target_lock(&format!("cache-lock-{index}"), &deadline)?;
            self.observe_directory_identity(
                &format!("cache-directory-{index}"),
                FileIdentity {
                    volume_or_device: 1,
                    file_or_inode: index as u64,
                },
                &deadline,
            )?;
        }
        Ok(self.cache_sizes_for_test())
    }

    fn directory_snapshot_path_with_deadline(
        &self,
        directory: Option<&RepoPath>,
        deadline: &OperationDeadline,
    ) -> Result<DirectorySnapshot, FileServiceError> {
        deadline.check()?;
        self.revalidate_root_with_deadline(deadline)?;
        let resolved = self.resolve_directory_with_deadline(directory, deadline)?;
        let read_directory = read_directory_from_handle(
            resolved.handle.as_ref().expect("resolved directory handle"),
            &resolved.full_path,
        )
        .map_err(|error| {
            self.io_error("list", directory.map_or("<root>", RepoPath::as_str), error)
        })?;
        let mut entries = Vec::with_capacity(MAX_LIST_ENTRIES.min(32));
        let mut entry_identities = Vec::with_capacity(MAX_LIST_ENTRIES.min(32));
        let mut physical_entries = 0_usize;
        for name_os in read_directory.into_iter().take(MAX_LIST_ENTRIES + 1) {
            physical_entries += 1;
            if physical_entries > MAX_LIST_ENTRIES {
                return Err(FileServiceError::ListOverflow {
                    limit: MAX_LIST_ENTRIES,
                });
            }
            deadline.check()?;
            let name = name_os
                .to_str()
                .ok_or_else(|| FileServiceError::InvalidPath {
                    path: "<directory-entry>".to_string(),
                    reason: "entry name is not valid UTF-8",
                })?;
            if is_private_cleanup_component(name) {
                continue;
            }
            let raw_path = directory.map_or_else(
                || name.to_string(),
                |dir| format!("{}/{}", dir.as_str(), name),
            );
            let path = normalize_relative_path(&raw_path)?;
            // The root was revalidated immediately before this handle-relative
            // snapshot and is revalidated again before the public operation
            // returns. Reopening the named root once per child adds no
            // authority: every child is opened from the already-pinned handle,
            // and a concurrent name replacement is caught by the final fence.
            let target =
                self.resolve_existing_from_validated_root_with_deadline(&path, deadline)?;
            let metadata = target.metadata.as_ref().expect("directory entry metadata");
            let target_identity = target.identity.ok_or(FileServiceError::RootUnavailable)?;
            let kind = if metadata.is_file() {
                EntryKind::File
            } else if metadata.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::Other
            };
            entries.push(FileMetadata {
                secret: classify_secret(path.as_str()),
                path,
                kind,
                byte_len: metadata.is_file().then(|| metadata.len()),
                modified_unix_nanos: modified_unix_nanos(metadata),
                permission_bits: permission_bits(metadata),
                content_kind: None,
            });
            entry_identities.push(target_identity);
        }
        if entries.len() > MAX_LIST_ENTRIES {
            return Err(FileServiceError::ListOverflow {
                limit: MAX_LIST_ENTRIES,
            });
        }
        let mut paired_entries = entries
            .into_iter()
            .zip(entry_identities)
            .collect::<Vec<_>>();
        paired_entries.sort_by(|(left, _), (right, _)| {
            left.path
                .as_str()
                .to_ascii_lowercase()
                .cmp(&right.path.as_str().to_ascii_lowercase())
                .then_with(|| left.path.cmp(&right.path))
        });
        let (entries, entry_identities): (Vec<_>, Vec<_>) = paired_entries.into_iter().unzip();
        let identity = resolved.identity.ok_or(FileServiceError::RootUnavailable)?;
        let revision = directory_revision(&identity, &entries, &entry_identities);
        let current = self.resolve_directory_with_deadline(directory, deadline)?;
        if current.identity != Some(identity) {
            return Err(FileServiceError::DirectoryChanged {
                path: directory.map_or_else(|| "<root>".to_string(), safe_path),
            });
        }
        deadline.check()?;
        Ok(DirectorySnapshot {
            entries,
            revision,
            identity,
        })
    }

    fn resolve_directory_with_deadline(
        &self,
        path: Option<&RepoPath>,
        deadline: &OperationDeadline,
    ) -> Result<ResolvedTarget, FileServiceError> {
        deadline.check()?;
        match path {
            Some(path) => {
                let resolved = self.resolve_existing_with_deadline(path, deadline)?;
                if !resolved.metadata.as_ref().is_some_and(fs::Metadata::is_dir) {
                    return Err(FileServiceError::NotDirectory {
                        path: safe_path(path),
                    });
                }
                Ok(resolved)
            }
            None => {
                let metadata = self
                    .root
                    .handle
                    .metadata()
                    .map_err(|_| FileServiceError::RootUnavailable)?;
                deadline.check()?;
                let handle = self
                    .root
                    .handle
                    .try_clone()
                    .map_err(|_| FileServiceError::RootUnavailable)?;
                deadline.check()?;
                Ok(ResolvedTarget {
                    full_path: self.root.path.clone(),
                    parent_path: self.root.path.clone(),
                    name: String::new(),
                    parent_identity: self.root.identity,
                    identity: Some(self.root.identity),
                    metadata: Some(metadata),
                    handle: Some(handle),
                    parent_write_handle: self
                        .root
                        .write_handle
                        .as_ref()
                        .and_then(|handle| handle.try_clone().ok()),
                })
            }
        }
    }

    fn resolve_existing_with_deadline(
        &self,
        path: &RepoPath,
        deadline: &OperationDeadline,
    ) -> Result<ResolvedTarget, FileServiceError> {
        self.resolve_target_with_deadline(path, false, deadline)
    }

    fn resolve_existing_from_validated_root_with_deadline(
        &self,
        path: &RepoPath,
        deadline: &OperationDeadline,
    ) -> Result<ResolvedTarget, FileServiceError> {
        self.resolve_target_from_validated_root_with_deadline(path, false, deadline)
    }

    fn resolve_target_with_deadline(
        &self,
        path: &RepoPath,
        allow_missing_final: bool,
        deadline: &OperationDeadline,
    ) -> Result<ResolvedTarget, FileServiceError> {
        deadline.check()?;
        self.revalidate_root_with_deadline(deadline)?;
        self.resolve_target_from_validated_root_with_deadline(path, allow_missing_final, deadline)
    }

    fn resolve_target_from_validated_root_with_deadline(
        &self,
        path: &RepoPath,
        allow_missing_final: bool,
        deadline: &OperationDeadline,
    ) -> Result<ResolvedTarget, FileServiceError> {
        deadline.check()?;
        if path.0.split('/').any(is_private_cleanup_component) {
            return Err(private_cleanup_not_found());
        }
        let components = path.0.split('/').collect::<Vec<_>>();
        let mut current_path = self.root.path.clone();
        let mut parent_path = self.root.path.clone();
        deadline.check()?;
        let mut parent_handle = self
            .root
            .handle
            .try_clone()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        deadline.check()?;
        let mut parent_write_handle = self
            .root
            .write_handle
            .as_ref()
            .and_then(|handle| handle.try_clone().ok());
        deadline.check()?;
        let mut parent_identity = self.root.identity;
        for (index, component) in components.iter().enumerate() {
            deadline.check()?;
            let is_final = index + 1 == components.len();
            current_path.push(component);
            deadline.check()?;
            let opened = match open_relative_nofollow(&parent_handle, &current_path, component) {
                Ok(opened) => opened,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && is_final
                        && allow_missing_final =>
                {
                    return Ok(ResolvedTarget {
                        full_path: current_path,
                        parent_path,
                        name: (*component).to_string(),
                        parent_identity,
                        identity: None,
                        metadata: None,
                        handle: None,
                        parent_write_handle,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(FileServiceError::NotFound {
                        path: safe_path(path),
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    return Err(FileServiceError::ReparseRejected {
                        path: safe_path(path),
                    })
                }
                Err(error) => return Err(self.io_error("open", path.as_str(), error)),
            };
            deadline.check()?;
            let opened_metadata = opened
                .metadata()
                .map_err(|error| self.io_error("stat opened target", path.as_str(), error))?;
            deadline.check()?;
            if metadata_is_reparse_point(&opened_metadata) {
                return Err(FileServiceError::ReparseRejected {
                    path: safe_path(path),
                });
            }
            let is_directory = opened_metadata.is_dir();
            if !is_final && !is_directory {
                return Err(FileServiceError::NotDirectory {
                    path: safe_path(path),
                });
            }
            if opened_metadata.is_dir() != is_directory {
                return Err(FileServiceError::ChangedDuringRead {
                    path: safe_path(path),
                });
            }
            deadline.check()?;
            let (identity, link_count) = opened_file_info(&opened)
                .map_err(|error| self.io_error("stat", path.as_str(), error))?;
            deadline.check()?;
            if !is_directory && link_count != 1 {
                return Err(FileServiceError::HardLinkRejected {
                    path: safe_path(path),
                });
            }
            if is_directory {
                self.observe_directory_identity(
                    &components[..=index].join("/"),
                    identity,
                    deadline,
                )?;
            }
            if !is_final {
                let opened_write_handle = match {
                    deadline.check()?;
                    open_child_nofollow_for_mutation(&parent_handle, component)
                } {
                    Ok(handle) => {
                        deadline.check()?;
                        let (write_identity, _) = opened_file_info(&handle).map_err(|error| {
                            self.io_error("stat opened parent", path.as_str(), error)
                        })?;
                        deadline.check()?;
                        if write_identity != identity {
                            return Err(FileServiceError::DirectoryChanged {
                                path: safe_path(path),
                            });
                        }
                        Some(handle)
                    }
                    Err(_) => None,
                };
                parent_path = current_path.clone();
                parent_handle = opened;
                parent_write_handle = opened_write_handle;
                parent_identity = identity;
                continue;
            }
            return Ok(ResolvedTarget {
                full_path: current_path,
                parent_path,
                name: (*component).to_string(),
                parent_identity,
                identity: Some(identity),
                metadata: Some(opened_metadata),
                handle: Some(opened),
                parent_write_handle,
            });
        }
        Err(FileServiceError::InvalidPath {
            path: safe_path(path),
            reason: "path has no components",
        })
    }

    fn revalidate_parent_within_root(
        &self,
        parent_path: &Path,
        expected_parent_identity: FileIdentity,
        deadline: &OperationDeadline,
    ) -> Result<(), FileServiceError> {
        deadline.check()?;
        self.revalidate_root_with_deadline(deadline)?;
        let relative = parent_path.strip_prefix(&self.root.path).map_err(|_| {
            FileServiceError::DirectoryChanged {
                path: "<parent>".to_string(),
            }
        })?;
        let mut current = self
            .root
            .handle
            .try_clone()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        let mut current_identity = self.root.identity;
        for component in relative.components() {
            deadline.check()?;
            let name = match component {
                std::path::Component::Normal(name) => {
                    name.to_str()
                        .ok_or_else(|| FileServiceError::DirectoryChanged {
                            path: "<parent>".to_string(),
                        })?
                }
                std::path::Component::CurDir => continue,
                _ => {
                    return Err(FileServiceError::DirectoryChanged {
                        path: "<parent>".to_string(),
                    })
                }
            };
            let opened = open_relative_nofollow(&current, parent_path, name).map_err(|_| {
                FileServiceError::DirectoryChanged {
                    path: "<parent>".to_string(),
                }
            })?;
            deadline.check()?;
            let metadata = opened
                .metadata()
                .map_err(|_| FileServiceError::RootUnavailable)?;
            if metadata_is_reparse_point(&metadata) || !metadata.is_dir() {
                return Err(FileServiceError::DirectoryChanged {
                    path: "<parent>".to_string(),
                });
            }
            current_identity = opened_file_info(&opened)
                .map_err(|_| FileServiceError::RootUnavailable)?
                .0;
            current = opened;
        }
        deadline.check()?;
        if current_identity != expected_parent_identity {
            return Err(FileServiceError::DirectoryChanged {
                path: "<parent>".to_string(),
            });
        }
        self.revalidate_root_with_deadline(deadline)
    }

    fn observe_directory_identity(
        &self,
        path: &str,
        identity: FileIdentity,
        deadline: &OperationDeadline,
    ) -> Result<(), FileServiceError> {
        let mut identities = self.directory_identities.lock_until(deadline)?;
        deadline.check()?;
        let key = path_key_text(path);
        if let Some(previous) = identities.get(&key) {
            if *previous != identity {
                return Err(FileServiceError::DirectoryChanged {
                    path: safe_path_text(path),
                });
            }
        } else {
            let mut order = self.directory_identity_order.lock_until(deadline)?;
            deadline.check()?;
            while identities.len() >= MAX_DIRECTORY_IDENTITIES {
                let Some(evicted) = order.pop_front() else {
                    identities.clear();
                    break;
                };
                identities.remove(&evicted);
            }
            identities.insert(key.clone(), identity);
            order.push_back(key);
        }
        Ok(())
    }

    fn current_expected_state_with_deadline(
        &self,
        path: &RepoPath,
        expected: &ExpectedRevision,
        resolved: &ResolvedTarget,
        deadline: &OperationDeadline,
    ) -> Result<Option<FileRevision>, FileServiceError> {
        deadline.check()?;
        if resolved.identity.is_none() {
            return Ok(None);
        }
        if !resolved
            .metadata
            .as_ref()
            .is_some_and(fs::Metadata::is_file)
        {
            return Err(FileServiceError::NotRegularFile {
                path: safe_path(path),
            });
        }
        self.revision_for_expected_with_deadline(path, expected, resolved, deadline)
            .map(Some)
    }

    fn revision_for_expected_with_deadline(
        &self,
        path: &RepoPath,
        _expected: &ExpectedRevision,
        resolved: &ResolvedTarget,
        deadline: &OperationDeadline,
    ) -> Result<FileRevision, FileServiceError> {
        deadline.check()?;
        let file = resolved.handle.as_ref().expect("resolved file handle");
        let revision =
            revision_from_opened_file_with_deadline(file, deadline).map_err(|error| {
                self.deadline_aware_io_error("hash mutation target", path.as_str(), error)
            })?;
        deadline.check()?;
        Ok(revision)
    }

    fn validate_record_target(
        &self,
        record: &MutationRecord,
        resolved: &ResolvedTarget,
    ) -> Result<(), FileServiceError> {
        if record.parent_identity != resolved.parent_identity
            || record.target_identity != resolved.identity
        {
            return Err(FileServiceError::Conflict {
                path: safe_path(&record.path),
            });
        }
        Ok(())
    }

    fn revalidate_root_with_deadline(
        &self,
        deadline: &OperationDeadline,
    ) -> Result<(), FileServiceError> {
        #[cfg(test)]
        self.root_revalidations.fetch_add(1, Ordering::AcqRel);
        // The descriptor remains the authority for traversal. This additional
        // metadata check only detects that the originally approved root name
        // was replaced or reparse-swapped. The production Task 6.2 binder
        // retains the verified final handle as the authority; the Windows
        // no-follow identity check below only revalidates the visible name.
        deadline.check()?;
        let named_metadata =
            fs::symlink_metadata(&self.root.path).map_err(|_| FileServiceError::RootUnavailable)?;
        deadline.check()?;
        if named_metadata.file_type().is_symlink()
            || metadata_is_reparse_point(&named_metadata)
            || !named_metadata.is_dir()
            || root_name_marker(&named_metadata) != self.root.name_marker
        {
            return Err(FileServiceError::RootUnavailable);
        }
        #[cfg(windows)]
        {
            // Stable Rust does not expose Windows file-index fields on
            // `Metadata`; bind the name check to a no-follow handle instead
            // of relying on creation time, which can collide for a rapid
            // same-name replacement. This is a revalidation check after the
            // bridge has already retained the authoritative final handle; it
            // does not establish the service binding from a path.
            deadline.check()?;
            let named_handle = open_nofollow(self.root.path.as_path(), true, false)
                .map_err(|_| FileServiceError::RootUnavailable)?;
            deadline.check()?;
            let named_metadata = named_handle
                .metadata()
                .map_err(|_| FileServiceError::RootUnavailable)?;
            deadline.check()?;
            if metadata_is_reparse_point(&named_metadata) || !named_metadata.is_dir() {
                return Err(FileServiceError::RootUnavailable);
            }
            deadline.check()?;
            let (named_identity, _) =
                opened_file_info(&named_handle).map_err(|_| FileServiceError::RootUnavailable)?;
            if named_identity != self.root.identity {
                return Err(FileServiceError::RootUnavailable);
            }
        }
        deadline.check()?;
        let metadata = self
            .root
            .handle
            .metadata()
            .map_err(|_| FileServiceError::RootUnavailable)?;
        deadline.check()?;
        if metadata_is_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(FileServiceError::RootUnavailable);
        }
        deadline.check()?;
        let (identity, _) =
            opened_file_info(&self.root.handle).map_err(|_| FileServiceError::RootUnavailable)?;
        deadline.check()?;
        if identity != self.root.identity {
            return Err(FileServiceError::RootUnavailable);
        }
        Ok(())
    }

    fn io_error(&self, operation: &'static str, path: &str, error: io::Error) -> FileServiceError {
        FileServiceError::Io {
            operation,
            path: safe_path_text(path),
            kind: error.kind(),
            raw_code: error.raw_os_error(),
        }
    }

    fn deadline_aware_io_error(
        &self,
        operation: &'static str,
        path: &str,
        error: io::Error,
    ) -> FileServiceError {
        if error.kind() == io::ErrorKind::TimedOut {
            FileServiceError::DeadlineExceeded
        } else {
            self.io_error(operation, path, error)
        }
    }
}

struct DirectorySnapshot {
    entries: Vec<FileMetadata>,
    revision: [u8; 32],
    identity: FileIdentity,
}

struct ResolvedTarget {
    full_path: PathBuf,
    parent_path: PathBuf,
    name: String,
    parent_identity: FileIdentity,
    identity: Option<FileIdentity>,
    metadata: Option<fs::Metadata>,
    handle: Option<File>,
    parent_write_handle: Option<File>,
}

#[cfg_attr(windows, allow(dead_code))]
struct TempCleanup {
    path: PathBuf,
    name: String,
    parent: File,
    temporary: File,
    temporary_identity: FileIdentity,
    armed: bool,
    accounting: Arc<CleanupLedger>,
    parent_identity: FileIdentity,
    deadline: OperationDeadline,
}

impl TempCleanup {
    fn from_temporary(
        temporary: &mut TemporaryFile,
        parent: File,
        temporary_handle: File,
        accounting: Arc<CleanupLedger>,
        parent_identity: FileIdentity,
        deadline: OperationDeadline,
    ) -> Self {
        let cleanup = Self {
            path: temporary.path.clone(),
            name: temporary.name.clone(),
            parent,
            temporary: temporary_handle,
            temporary_identity: temporary.identity,
            armed: true,
            accounting,
            parent_identity,
            deadline,
        };
        // Ownership transfers exactly once. If any later setup step fails,
        // this guard is the sole cleanup owner; TemporaryFile cannot double
        // delete or quarantine the same private name.
        temporary.disarm();
        cleanup
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(test)]
        TEST_ARMED_CLEANUP_DROPS.fetch_add(1, Ordering::AcqRel);
        #[cfg(unix)]
        {
            if self.deadline.check().is_err() {
                #[cfg(target_os = "linux")]
                record_uncertain_cleanup_nonblocking(
                    &self.accounting,
                    &self.parent,
                    self.parent_identity,
                    &self.name,
                    self.temporary_identity,
                );
                return;
            }
            if unlink_private_name_if_identity(
                &self.parent,
                &self.name,
                self.temporary_identity,
                &self.accounting,
                &self.deadline,
            )
            .is_err()
            {
                #[cfg(target_os = "linux")]
                {
                    // A failed unlink can leave a private inode behind. It
                    // may be quarantined only after the same bounded ledger
                    // reserves one of the 64 cleanup slots used by explicit
                    // mutations. At capacity, leave the exact private name
                    // visible and fail closed.
                    if let Some(mut reservation) = reserve_cleanup_slot(&self.accounting) {
                        if self.deadline.check().is_err() {
                            insert_reserved_cleanup_record_nonblocking_from_parts(
                                &self.accounting,
                                &mut reservation,
                                &self.parent,
                                self.parent_identity,
                                self.name.clone(),
                                self.temporary_identity,
                                true,
                            );
                        } else if let Ok(tombstone) = quarantine_private_temporary(
                            &self.parent,
                            &self.name,
                            self.temporary_identity,
                            &self.deadline,
                        ) {
                            insert_reserved_cleanup_record_nonblocking_from_parts(
                                &self.accounting,
                                &mut reservation,
                                &self.parent,
                                self.parent_identity,
                                tombstone,
                                self.temporary_identity,
                                false,
                            );
                        } else {
                            insert_reserved_cleanup_record_nonblocking_from_parts(
                                &self.accounting,
                                &mut reservation,
                                &self.parent,
                                self.parent_identity,
                                self.name.clone(),
                                self.temporary_identity,
                                true,
                            );
                        }
                    } else {
                        record_uncertain_cleanup_nonblocking(
                            &self.accounting,
                            &self.parent,
                            self.parent_identity,
                            &self.name,
                            self.temporary_identity,
                        );
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            if self.deadline.check().is_ok() && delete_opened_file(&self.temporary).is_ok() {
                return;
            }
            record_uncertain_cleanup_nonblocking(
                &self.accounting,
                &self.parent,
                self.parent_identity,
                &self.name,
                self.temporary_identity,
            );
        }
        #[cfg(not(any(unix, windows)))]
        {
            if self.deadline.check().is_ok() && fs::remove_file(&self.path).is_ok() {
                return;
            }
            record_uncertain_cleanup_nonblocking(
                &self.accounting,
                &self.parent,
                self.parent_identity,
                &self.name,
                self.temporary_identity,
            );
        }
    }
}

struct TemporaryFile {
    path: PathBuf,
    name: String,
    parent: File,
    file: File,
    identity: FileIdentity,
    armed: bool,
    accounting: Arc<CleanupLedger>,
    parent_identity: FileIdentity,
    deadline: OperationDeadline,
}

impl TemporaryFile {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(test)]
        TEST_ARMED_CLEANUP_DROPS.fetch_add(1, Ordering::AcqRel);
        // The name is private and was created with O_EXCL/FILE_CREATE. If
        // setup fails before TempCleanup is armed, remove only that private
        // candidate; never follow an attacker-controlled path.
        #[cfg(any(unix, windows))]
        {
            #[cfg(windows)]
            {
                // The create handle was opened with DELETE access. Delete
                // that held inode directly; reopening the pathname would
                // reintroduce a name race during setup-failure cleanup.
                if self.deadline.check().is_ok() && delete_opened_file(&self.file).is_ok() {
                    return;
                }
                record_uncertain_cleanup_nonblocking(
                    &self.accounting,
                    &self.parent,
                    self.parent_identity,
                    &self.name,
                    self.identity,
                );
            }
            #[cfg(target_os = "linux")]
            {
                if self.deadline.check().is_err() {
                    record_uncertain_cleanup_nonblocking(
                        &self.accounting,
                        &self.parent,
                        self.parent_identity,
                        &self.name,
                        self.identity,
                    );
                    return;
                }
                {
                    // Quarantine first while the private create handle still
                    // proves the inode. If cleanup then fails, the generated
                    // tombstone remains visible for bounded startup recovery;
                    // there is no hidden best-effort pathname leak before the
                    // service's TempCleanup guard is armed.
                    if let Some(mut reservation) = reserve_cleanup_slot(&self.accounting) {
                        if self.deadline.check().is_ok() {
                            if let Ok(tombstone) = quarantine_private_temporary(
                                &self.parent,
                                &self.name,
                                self.identity,
                                &self.deadline,
                            ) {
                                if self.deadline.check().is_err() {
                                    insert_reserved_cleanup_record_nonblocking_from_parts(
                                        &self.accounting,
                                        &mut reservation,
                                        &self.parent,
                                        self.parent_identity,
                                        tombstone,
                                        self.identity,
                                        false,
                                    );
                                } else if unlink_private_name_if_identity(
                                    &self.parent,
                                    &tombstone,
                                    self.identity,
                                    &self.accounting,
                                    &self.deadline,
                                )
                                .is_err()
                                {
                                    insert_reserved_cleanup_record_nonblocking_from_parts(
                                        &self.accounting,
                                        &mut reservation,
                                        &self.parent,
                                        self.parent_identity,
                                        tombstone,
                                        self.identity,
                                        false,
                                    );
                                } else {
                                    reservation.release();
                                }
                            } else {
                                insert_reserved_cleanup_record_nonblocking_from_parts(
                                    &self.accounting,
                                    &mut reservation,
                                    &self.parent,
                                    self.parent_identity,
                                    self.name.clone(),
                                    self.identity,
                                    true,
                                );
                            }
                        } else {
                            insert_reserved_cleanup_record_nonblocking_from_parts(
                                &self.accounting,
                                &mut reservation,
                                &self.parent,
                                self.parent_identity,
                                self.name.clone(),
                                self.identity,
                                true,
                            );
                        }
                    } else {
                        record_uncertain_cleanup_nonblocking(
                            &self.accounting,
                            &self.parent,
                            self.parent_identity,
                            &self.name,
                            self.identity,
                        );
                    }
                }
            }
            #[cfg(all(unix, not(target_os = "linux")))]
            {
                if self.deadline.check().is_ok()
                    && opened_file_info(&self.file)
                        .ok()
                        .and_then(|(identity, _)| {
                            unlink_private_name_if_identity(
                                &self.parent,
                                &self.name,
                                identity,
                                &self.accounting,
                                &self.deadline,
                            )
                            .ok()
                        })
                        .is_some()
                {
                    return;
                }
                record_uncertain_cleanup_nonblocking(
                    &self.accounting,
                    &self.parent,
                    self.parent_identity,
                    &self.name,
                    self.identity,
                );
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            if self.deadline.check().is_ok() && fs::remove_file(&self.path).is_ok() {
                return;
            }
            record_uncertain_cleanup_nonblocking(
                &self.accounting,
                &self.parent,
                self.parent_identity,
                &self.name,
                self.identity,
            );
        }
    }
}

#[derive(Clone)]
struct OpenedFingerprint {
    fingerprint: FileFingerprint,
    identity: FileIdentity,
}

#[cfg(test)]
fn open_approved_root(root: &Path) -> Result<OpenedRoot, FileServiceError> {
    // The bridge pins this descriptor once. Every ancestor is opened
    // relative to the previously verified directory handle, so an initial
    // junction/symlink cannot redirect the approved root before it is pinned.
    #[cfg(unix)]
    let handle = {
        use std::path::Component;
        if !root.is_absolute() {
            return Err(FileServiceError::RootUnavailable);
        }
        let mut handle = open_nofollow(Path::new("/"), true, true)
            .map_err(|_| FileServiceError::RootUnavailable)?;
        for component in root.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let name = name.to_str().ok_or(FileServiceError::RootUnavailable)?;
                    handle = open_child_nofollow(&handle, name)
                        .map_err(|_| FileServiceError::RootUnavailable)?;
                }
                Component::CurDir => {}
                Component::Prefix(_) | Component::ParentDir => {
                    return Err(FileServiceError::RootUnavailable)
                }
            }
        }
        handle
    };
    #[cfg(windows)]
    let handle = {
        use std::path::Component;
        let mut components = root.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(FileServiceError::RootUnavailable);
        };
        let mut volume_root = PathBuf::from(prefix.as_os_str());
        volume_root.push("\\");
        let mut verified_handle = open_nofollow(&volume_root, true, false)
            .map_err(|_| FileServiceError::RootUnavailable)?;
        for component in components {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    let name = name.to_str().ok_or(FileServiceError::RootUnavailable)?;
                    verified_handle = open_child_nofollow(&verified_handle, name)
                        .map_err(|_| FileServiceError::RootUnavailable)?;
                }
                Component::Prefix(_) | Component::ParentDir => {
                    return Err(FileServiceError::RootUnavailable)
                }
            }
        }
        let _ = verified_handle;
        open_nofollow(root, true, true).map_err(|_| FileServiceError::RootUnavailable)?
    };
    #[cfg(not(any(unix, windows)))]
    let handle = open_nofollow(root, true, false).map_err(|_| FileServiceError::RootUnavailable)?;
    let metadata = handle
        .metadata()
        .map_err(|_| FileServiceError::RootUnavailable)?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(FileServiceError::RootUnavailable);
    }
    let (identity, _) = opened_file_info(&handle).map_err(|_| FileServiceError::RootUnavailable)?;
    let name_marker = root_name_marker(&metadata);
    let path = root.to_path_buf();
    let write_handle = handle.try_clone().ok();
    Ok(OpenedRoot {
        path,
        handle,
        write_handle,
        identity,
        name_marker,
    })
}

fn normalize_relative_path(raw: &str) -> Result<RepoPath, FileServiceError> {
    if raw.is_empty() {
        return Err(FileServiceError::InvalidPath {
            path: "<empty-path>".to_string(),
            reason: "path is empty",
        });
    }
    if raw.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(FileServiceError::InvalidPath {
            path: "<path-too-long>".to_string(),
            reason: "path exceeds the byte bound",
        });
    }
    if raw.chars().count() > MAX_RELATIVE_PATH_CHARS {
        return Err(FileServiceError::InvalidPath {
            path: "<path-too-long>".to_string(),
            reason: "path exceeds the character bound",
        });
    }
    let safe = safe_path_text(raw);
    if !raw.is_ascii()
        || raw.chars().any(char::is_control)
        || raw.contains(':')
        || raw.starts_with('/')
        || raw.starts_with('\\')
    {
        return Err(FileServiceError::InvalidPath {
            path: safe,
            reason: if !raw.is_ascii() {
                "path contains ambiguous Unicode"
            } else {
                "path is not a strict relative repository path"
            },
        });
    }
    let mut components = Vec::new();
    let normalized = raw.replace('\\', "/");
    for component in normalized.split('/') {
        if component.is_empty() {
            return Err(FileServiceError::InvalidPath {
                path: safe,
                reason: "path contains an empty component",
            });
        }
        if component == "." || component == ".." {
            return Err(FileServiceError::InvalidPath {
                path: safe,
                reason: "dot traversal components are forbidden",
            });
        }
        if component.len() > MAX_COMPONENT_BYTES
            || component.ends_with('.')
            || component.ends_with(' ')
            || component
                .chars()
                .any(|character| matches!(character, '<' | '>' | '"' | '|' | '?' | '*'))
            || is_reserved_windows_name(component)
        {
            return Err(FileServiceError::InvalidPath {
                path: safe,
                reason: "component is unsafe on the Windows host",
            });
        }
        components.push(component);
    }
    Ok(RepoPath::new(components.join("/")))
}

fn is_reserved_windows_name(component: &str) -> bool {
    let base = component
        .trim_end_matches(['.', ' '])
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || (base.len() == 4
        && (base.starts_with("com") || base.starts_with("lpt"))
        && base.as_bytes()[3].is_ascii_digit()
        && base.as_bytes()[3] != b'0')
}

fn classify_secret(path: &str) -> SecretClassification {
    if path.len() > MAX_RELATIVE_PATH_BYTES || path.chars().count() > MAX_RELATIVE_PATH_CHARS {
        return SecretClassification::Ordinary;
    }
    for component in path.split(['/', '\\']) {
        if component.len() > MAX_COMPONENT_BYTES {
            return SecretClassification::Ordinary;
        }
        let lower = component.to_ascii_lowercase();
        let trimmed = lower.trim_end_matches(['.', ' ']);
        if trimmed == ".env"
            || trimmed.starts_with(".env.")
            || trimmed.contains("credential")
            || trimmed.contains("secret")
            || trimmed.contains("password")
            || trimmed.contains("passwd")
            || trimmed.contains("token")
            || trimmed.contains("private")
            || trimmed.ends_with(".pem")
            || trimmed.ends_with(".key")
            || trimmed.ends_with(".p12")
            || trimmed.ends_with(".pfx")
            || trimmed == "id_rsa"
            || trimmed == "id_ed25519"
        {
            return SecretClassification::SecretLike;
        }
    }
    SecretClassification::Ordinary
}

fn classify_content(bytes: &[u8]) -> ContentKind {
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        ContentKind::Binary
    } else {
        ContentKind::Text
    }
}

fn chunks_to_body(chunks: &[ReadChunk]) -> Vec<u8> {
    let total = chunks.iter().map(|chunk| chunk.bytes.len()).sum();
    let mut body = Vec::with_capacity(total);
    for chunk in chunks {
        body.extend_from_slice(&chunk.bytes);
    }
    body
}

fn line_text(bytes: &[u8], path: &RepoPath) -> Result<String, FileServiceError> {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).map_err(|_| FileServiceError::BinaryContent {
        path: safe_path(path),
    })
}

fn safe_path(path: &RepoPath) -> String {
    safe_path_text(path.as_str())
}

fn safe_path_text(path: &str) -> String {
    if path.len() > MAX_RELATIVE_PATH_BYTES || path.chars().count() > MAX_RELATIVE_PATH_CHARS {
        return "<path-redacted>".to_string();
    }
    if classify_secret(path) == SecretClassification::SecretLike {
        "<secret-like-path>".to_string()
    } else {
        path.to_string()
    }
}

fn ensure_expected(
    expected: &ExpectedRevision,
    actual: Option<&FileRevision>,
    path: &RepoPath,
) -> Result<(), FileServiceError> {
    let matches = match (expected, actual) {
        (ExpectedRevision::Missing, None) => true,
        (
            ExpectedRevision::Exact {
                fingerprint,
                sha256,
            },
            Some(actual),
        ) => {
            &actual.fingerprint == fingerprint
                && sha256.is_none_or(|expected_sha| actual.sha256 == Some(expected_sha))
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(FileServiceError::Conflict {
            path: safe_path(path),
        })
    }
}

fn file_fingerprint(file: &File) -> io::Result<OpenedFingerprint> {
    let metadata = file.metadata()?;
    let (identity, _) = opened_file_info(file)?;
    Ok(OpenedFingerprint {
        fingerprint: FileFingerprint {
            byte_len: metadata.len(),
            modified_unix_nanos: modified_unix_nanos(&metadata),
            permission_bits: permission_bits(&metadata),
            identity,
        },
        identity,
    })
}

fn deadline_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "workspace operation deadline exceeded",
    )
}

fn check_deadline_io(deadline: &OperationDeadline) -> io::Result<()> {
    deadline.check().map_err(|_| deadline_io_error())
}

fn revision_from_opened_file_with_deadline(
    file: &File,
    deadline: &OperationDeadline,
) -> io::Result<FileRevision> {
    check_deadline_io(deadline)?;
    let before = file_fingerprint(file)?;
    check_deadline_io(deadline)?;
    if before.fingerprint.byte_len > MAX_READ_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "mutation target exceeds the read bound",
        ));
    }
    check_deadline_io(deadline)?;
    let mut reader = file.try_clone()?;
    check_deadline_io(deadline)?;
    reader.seek(SeekFrom::Start(0))?;
    check_deadline_io(deadline)?;
    let mut body = Vec::with_capacity(before.fingerprint.byte_len as usize);
    let mut reader = reader.take(MAX_READ_BYTES as u64 + 1);
    loop {
        check_deadline_io(deadline)?;
        let mut chunk = [0_u8; MAX_CHUNK_BYTES];
        let read = reader.read(&mut chunk)?;
        check_deadline_io(deadline)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() > MAX_READ_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "mutation target exceeds the read bound",
            ));
        }
    }
    if body.len() > MAX_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "mutation target exceeds the read bound",
        ));
    }
    check_deadline_io(deadline)?;
    let after = file_fingerprint(file)?;
    check_deadline_io(deadline)?;
    if before.fingerprint != after.fingerprint || body.len() as u64 != after.fingerprint.byte_len {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "mutation target changed while hashing",
        ));
    }
    Ok(FileRevision {
        fingerprint: after.fingerprint,
        sha256: Some(Sha256::digest(&body).into()),
    })
}

fn permission_bits(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o7777;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes() & 0x1;
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata.permissions().readonly() as u32
    }
}

fn modified_unix_nanos(metadata: &fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
}

fn root_name_marker(metadata: &fs::Metadata) -> RootNameMarker {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return RootNameMarker {
            creation_time: metadata.creation_time(),
            file_attributes: metadata.file_attributes(),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return RootNameMarker {
            identity: FileIdentity {
                volume_or_device: metadata.dev(),
                file_or_inode: metadata.ino(),
            },
        };
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        RootNameMarker {}
    }
}

fn directory_revision(
    identity: &FileIdentity,
    entries: &[FileMetadata],
    entry_identities: &[FileIdentity],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(identity.volume_or_device.to_le_bytes());
    hasher.update(identity.file_or_inode.to_le_bytes());
    for (entry, entry_identity) in entries.iter().zip(entry_identities) {
        hasher.update(entry.path.as_str().as_bytes());
        hasher.update([0]);
        hasher.update([entry.kind as u8]);
        hasher.update(entry.byte_len.unwrap_or_default().to_le_bytes());
        hasher.update(entry.modified_unix_nanos.unwrap_or_default().to_le_bytes());
        hasher.update(entry.permission_bits.to_le_bytes());
        hasher.update(entry_identity.volume_or_device.to_le_bytes());
        hasher.update(entry_identity.file_or_inode.to_le_bytes());
    }
    hasher.finalize().into()
}

fn preserve_permissions(
    original: Option<(&fs::Metadata, &File)>,
    temporary: &File,
    path: &RepoPath,
    deadline: &OperationDeadline,
) -> Result<(), FileServiceError> {
    deadline.check()?;
    let Some((original, original_file)) = original else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        temporary
            .set_permissions(fs::Permissions::from_mode(original.mode() & 0o7777))
            .map_err(|_| FileServiceError::PermissionPreservationFailed {
                path: safe_path(path),
            })?;
        deadline.check()?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let readonly = original.file_attributes() & 0x1 != 0;
        let mut permissions = temporary
            .metadata()
            .map_err(|_| FileServiceError::PermissionPreservationFailed {
                path: safe_path(path),
            })?
            .permissions();
        deadline.check()?;
        permissions.set_readonly(readonly);
        temporary.set_permissions(permissions).map_err(|_| {
            FileServiceError::PermissionPreservationFailed {
                path: safe_path(path),
            }
        })?;
        deadline.check()?;
        preserve_windows_acl(original_file, temporary, path, deadline)?;
    }
    Ok(())
}

#[cfg(windows)]
fn preserve_windows_acl(
    original: &File,
    temporary: &File,
    path: &RepoPath,
    deadline: &OperationDeadline,
) -> Result<(), FileServiceError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    deadline.check()?;
    let mut dacl = std::ptr::null_mut::<ACL>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(original.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    };
    deadline.check()?;
    if status.0 != 0 {
        return Err(FileServiceError::PermissionPreservationFailed {
            path: safe_path(path),
        });
    }
    let status = unsafe {
        SetSecurityInfo(
            HANDLE(temporary.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl as *const ACL),
            None,
        )
    };
    let deadline_result = deadline.check();
    unsafe {
        windows::Win32::Foundation::LocalFree(Some(HLOCAL(descriptor.0)));
    }
    deadline_result?;
    if status.0 != 0 {
        return Err(FileServiceError::PermissionPreservationFailed {
            path: safe_path(path),
        });
    }
    Ok(())
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn opened_file_info(file: &File) -> io::Result<(FileIdentity, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let ok =
            unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info).is_ok() };
        if !ok {
            return Err(io::Error::last_os_error());
        }
        return Ok((
            FileIdentity {
                volume_or_device: info.dwVolumeSerialNumber as u64,
                file_or_inode: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            },
            info.nNumberOfLinks as u64,
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        return Ok((
            FileIdentity {
                volume_or_device: metadata.dev(),
                file_or_inode: metadata.ino(),
            },
            metadata.nlink(),
        ));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let metadata = file.metadata()?;
        Ok((
            FileIdentity {
                volume_or_device: 0,
                file_or_inode: metadata.len(),
            },
            1,
        ))
    }
}

fn open_nofollow(path: &Path, directory: bool, write: bool) -> io::Result<File> {
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = (path, directory, write);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no-follow descriptor opens are unsupported on this Unix target",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(write);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let flags = 0x0020_0000_u32 | if directory { 0x0200_0000 } else { 0 };
        options.custom_flags(flags);
        // Keep all sharing modes enabled so the atomic same-volume replacement
        // can proceed while final validation handles remain open.
        options.share_mode(0x0000_0007);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(unix_no_follow_flags(directory));
    }
    options.open(path)
}

fn open_relative_nofollow(parent: &File, full_path: &Path, name: &str) -> io::Result<File> {
    #[cfg(unix)]
    {
        return open_child_nofollow(parent, name);
    }
    #[cfg(windows)]
    {
        let _ = full_path;
        return open_child_nofollow(parent, name);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (parent, name);
        open_nofollow(full_path, false, false)
    }
}

#[cfg(unix)]
fn open_child_nofollow(parent: &File, name: &str) -> io::Result<File> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (parent, name);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no-follow descriptor opens are unsupported on this Unix target",
        ));
    }
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = CString::new(name).map_err(io::Error::other)?;
    let fd = unsafe {
        unix_at::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            unix_o_cloexec() | unix_o_nofollow() | unix_o_nonblock(),
            0,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(40) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reparse or symlink target",
            ));
        }
        return Err(error);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_child_nofollow_for_mutation(parent: &File, name: &str) -> io::Result<File> {
    // Keep the mutation descriptor for the verified child directory. A clone
    // of the ancestor would let a nested write address the wrong parent.
    open_child_nofollow(parent, name)
}

#[cfg(windows)]
fn open_child_nofollow(parent: &File, name: &str) -> io::Result<File> {
    open_child_nofollow_with_access(parent, name, 0, 0x0000_0007)
}

#[cfg(windows)]
fn open_child_nofollow_for_mutation(parent: &File, name: &str) -> io::Result<File> {
    open_child_nofollow_with_access(parent, name, 0x0000_0046, 0x0000_0007)
}

#[cfg(windows)]
fn open_child_nofollow_for_delete(parent: &File, name: &str) -> io::Result<File> {
    open_child_nofollow_with_access(parent, name, 0x0001_0000, 0x0000_0001)
}

#[cfg(windows)]
fn open_child_nofollow_for_cleanup(parent: &File, name: &str) -> io::Result<File> {
    // Startup recovery must reopen a tombstone with DELETE access. Sharing
    // read+delete prevents a concurrent writer from changing the validated
    // handle while still allowing repeated service startups to reclaim it.
    open_child_nofollow_with_access(parent, name, 0x0001_0000, 0x0000_0005)
}

#[cfg(windows)]
fn open_child_nofollow_for_cas(parent: &File, name: &str) -> io::Result<File> {
    // Request delete access so the validated handle can be detached to its
    // private tombstone. Refuse new writers while retaining delete sharing for
    // the handle-relative rename itself.
    open_child_nofollow_with_access(parent, name, 0x0001_0000, 0x0000_0005)
}

#[cfg(windows)]
fn open_child_nofollow_with_access(
    parent: &File,
    name: &str,
    extra_access: u32,
    share_access: u32,
) -> io::Result<File> {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    #[repr(C)]
    struct NtUnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct NtObjectAttributes {
        length: u32,
        root_directory: *mut c_void,
        object_name: *mut NtUnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct NtIoStatusBlock {
        status: i32,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut *mut c_void,
            desired_access: u32,
            object_attributes: *const NtObjectAttributes,
            io_status_block: *mut NtIoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    let utf16 = name.encode_utf16().collect::<Vec<_>>();
    let byte_len = utf16
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "component is too long"))?;
    let byte_len = u16::try_from(byte_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "component is too long"))?;
    let mut unicode = NtUnicodeString {
        length: byte_len,
        maximum_length: byte_len,
        buffer: utf16.as_ptr().cast_mut(),
    };
    let attributes = NtObjectAttributes {
        length: std::mem::size_of::<NtObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &mut unicode,
        attributes: 0x0000_1040, // OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut status_block = NtIoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // FILE_LIST_DIRECTORY/FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL
    // | SYNCHRONIZE. A mutation parent additionally requests FILE_ADD_FILE,
    // FILE_ADD_SUBDIRECTORY, and DELETE_CHILD for handle-relative rename and
    // unlink operations.
    // FILE_OPEN_REPARSE_POINT and FILE_SYNCHRONOUS_IO_NONALERT keep traversal
    // relative to the verified directory handle and inspect reparse points
    // instead of following them.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            0x0012_0081 | extra_access,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            share_access,
            0x0000_0001, // FILE_OPEN
            0x0020_0020,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(if code == 0 {
            31
        } else {
            code as i32
        }));
    }
    if handle.is_null() {
        return Err(io::Error::other("Windows returned an empty file handle"));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(not(any(unix, windows)))]
fn open_child_nofollow_for_mutation(_parent: &File, _name: &str) -> io::Result<File> {
    Err(io::Error::other(
        "handle-relative mutation is unsupported on this platform",
    ))
}

fn read_directory_from_handle(handle: &File, fallback: &Path) -> io::Result<Vec<OsString>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let _ = fallback;
        // Linux and macOS expose descriptor-relative directory views under
        // different namespaces. Never construct a macOS path through Linux's
        // `/proc`; `/dev/fd` is the supported Darwin view and remains bound
        // to the already-open directory handle.
        #[cfg(target_os = "macos")]
        let fd_namespace = "/dev/fd";
        #[cfg(not(target_os = "macos"))]
        let fd_namespace = "/proc/self/fd";
        let path = PathBuf::from(format!("{fd_namespace}/{}", handle.as_raw_fd()));
        return fs::read_dir(path)?
            .take(MAX_LIST_ENTRIES + 1)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect();
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::io::AsRawHandle;
        let _ = fallback;
        use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
        use windows::Win32::Storage::FileSystem::{
            FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx,
            FILE_ID_BOTH_DIR_INFO,
        };
        let mut entries = Vec::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut first_query = true;
        loop {
            let result = unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(handle.as_raw_handle()),
                    if first_query {
                        FileIdBothDirectoryRestartInfo
                    } else {
                        FileIdBothDirectoryInfo
                    },
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                )
            };
            first_query = false;
            if let Err(error) = result {
                if (error.code().0 as u32 & 0xffff) == ERROR_NO_MORE_FILES.0 {
                    return Ok(entries);
                }
                return Err(io::Error::from_raw_os_error(error.code().0));
            }
            let mut offset = 0_usize;
            loop {
                let file_name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
                if offset
                    .checked_add(file_name_offset)
                    .is_none_or(|end| end > buffer.len())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid verified directory record",
                    ));
                }
                let info = unsafe {
                    buffer
                        .as_ptr()
                        .add(offset)
                        .cast::<FILE_ID_BOTH_DIR_INFO>()
                        .read_unaligned()
                };
                let name_bytes = usize::try_from(info.FileNameLength).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "directory name is too long")
                })?;
                if name_bytes % std::mem::size_of::<u16>() != 0
                    || offset
                        .checked_add(file_name_offset)
                        .and_then(|start| start.checked_add(name_bytes))
                        .is_none_or(|end| end > buffer.len())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid verified directory name",
                    ));
                }
                let name = unsafe {
                    std::slice::from_raw_parts(
                        std::ptr::addr_of!(
                            (*buffer.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>()).FileName
                        )
                        .cast::<u16>(),
                        name_bytes / std::mem::size_of::<u16>(),
                    )
                };
                if name != ['.' as u16].as_slice() && name != ['.' as u16, '.' as u16].as_slice() {
                    entries.push(OsString::from_wide(name));
                    if entries.len() >= MAX_LIST_ENTRIES + 1 {
                        return Ok(entries);
                    }
                }
                let next = usize::try_from(info.NextEntryOffset).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid directory offset")
                })?;
                if next == 0 {
                    break;
                }
                if next < file_name_offset
                    || offset
                        .checked_add(next)
                        .is_none_or(|end| end > buffer.len())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid directory record offset",
                    ));
                }
                offset += next;
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = handle;
        fs::read_dir(fallback)?
            .take(MAX_LIST_ENTRIES + 1)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }
}

#[cfg(unix)]
fn unix_no_follow_flags(directory: bool) -> i32 {
    let mut flags = unix_o_nofollow() | unix_o_cloexec();
    if directory {
        flags |= unix_o_directory();
    }
    flags
}

#[cfg(unix)]
fn unix_o_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0x20000
    }
    #[cfg(target_os = "macos")]
    {
        0x100
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        panic!("unix no-follow flag is unavailable on this target")
    }
}

#[cfg(unix)]
fn unix_o_cloexec() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0x80000
    }
    #[cfg(target_os = "macos")]
    {
        0x1000000
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(unix)]
fn unix_o_directory() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0x10000
    }
    #[cfg(target_os = "macos")]
    {
        0x100000
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(unix)]
fn unix_o_nonblock() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0x800
    }
    #[cfg(target_os = "macos")]
    {
        0x4
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(unix)]
mod unix_at {
    use std::os::raw::{c_char, c_int};

    extern "C" {
        pub fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: u32) -> c_int;
        pub fn renameat(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
        ) -> c_int;
        #[cfg(target_os = "linux")]
        pub fn renameat2(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: u32,
        ) -> c_int;
        pub fn linkat(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: c_int,
        ) -> c_int;
        pub fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int;
    }
}

fn create_sibling_temp(
    _parent: &File,
    parent_path: &Path,
    accounting: Arc<CleanupLedger>,
    parent_identity: FileIdentity,
    expected_target_identity: Option<FileIdentity>,
    deadline: &OperationDeadline,
) -> io::Result<TemporaryFile> {
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = (_parent, parent_path);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic temporary files are unsupported on this Unix target",
        ));
    }
    for _ in 0..32 {
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let cleanup_parent = _parent.try_clone()?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let mut random = [0_u8; 16];
        fill_random(&mut random).map_err(io::Error::other)?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let target = expected_target_identity.unwrap_or(FileIdentity {
            volume_or_device: 0,
            file_or_inode: 0,
        });
        // The intent name is durable enough for restart discovery to see that
        // an operation was in flight, but deliberately is not a recoverable
        // cleanup name until the exact created identity has been recorded by
        // the handle-relative rename below.
        let intent_name = format!(
            ".devmanager-file-intent-{:016x}-{:016x}-{:016x}-{:016x}-{}.tmp",
            parent_identity.volume_or_device,
            parent_identity.file_or_inode,
            target.volume_or_device,
            target.file_or_inode,
            encode_nonce(&random),
        );
        #[cfg(unix)]
        let file = {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};
            let name_c = CString::new(intent_name.as_str()).map_err(io::Error::other)?;
            let flags = 0x40 | 0x80 | unix_o_cloexec() | unix_o_nofollow();
            let fd = unsafe { unix_at::openat(_parent.as_raw_fd(), name_c.as_ptr(), flags, 0o600) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error);
            }
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = {
            match create_child_exclusive(_parent, &intent_name) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let identity = match opened_file_info(&file) {
            Ok((identity, _)) => identity,
            Err(error) => return Err(error),
        };
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let name =
            bound_temporary_name(parent_identity, expected_target_identity, identity, &random);
        if let Err(error) = rename_private_intent(_parent, &intent_name, &name, &file, deadline) {
            // Keep the unbound intent visible on failure. It is not eligible
            // for restart deletion and therefore cannot accidentally consume
            // a replacement inode under the same name.
            drop(file);
            return Err(error);
        }
        let temporary = TemporaryFile {
            path: parent_path.join(&name),
            name,
            parent: cleanup_parent,
            file,
            identity,
            armed: true,
            accounting,
            parent_identity,
            deadline: deadline.clone(),
        };
        if deadline.check().is_err() {
            drop(temporary);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            ));
        }
        return Ok(temporary);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a workspace temporary file",
    ))
}

#[cfg(target_os = "linux")]
fn rename_private_intent(
    parent: &File,
    intent_name: &str,
    bound_name: &str,
    file: &File,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    deadline.check().map_err(|_| deadline_io_error())?;
    let expected_identity = opened_file_info(file)?.0;
    deadline.check().map_err(|_| deadline_io_error())?;
    let observed = open_child_nofollow(parent, intent_name)
        .and_then(|candidate| opened_file_info(&candidate))?;
    deadline.check().map_err(|_| deadline_io_error())?;
    if observed.0 != expected_identity || observed.1 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary intent identity changed",
        ));
    }
    let intent = CString::new(intent_name).map_err(io::Error::other)?;
    let bound = CString::new(bound_name).map_err(io::Error::other)?;
    let empty = CString::new("").expect("empty C string");
    const AT_EMPTY_PATH: i32 = 0x1000;
    let result = unsafe {
        unix_at::linkat(
            file.as_raw_fd(),
            empty.as_ptr(),
            parent.as_raw_fd(),
            bound.as_ptr(),
            AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        ) {
            return Err(error);
        }
        // AT_EMPTY_PATH requires a capability on some Linux configurations.
        // Fall back to the already-validated intent pathname and immediately
        // re-open the bound name by identity before exposing the temporary.
        let result = unsafe {
            unix_at::linkat(
                parent.as_raw_fd(),
                intent.as_ptr(),
                parent.as_raw_fd(),
                bound.as_ptr(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // The descriptor-bound link is committed. Any deadline or observation
    // failure from this point must still return an armed TemporaryFile so its
    // exact identity-bound name is handed to the single Drop guard; returning
    // an ordinary error here would leave an anonymous private inode behind.
    if deadline.check().is_err() {
        return Ok(());
    }
    let bound_observed = match open_child_nofollow(parent, bound_name)
        .and_then(|candidate| opened_file_info(&candidate))
    {
        Ok(observed) => observed,
        Err(_) => return Ok(()),
    };
    if bound_observed.0 != expected_identity || bound_observed.1 == 0 {
        return Ok(());
    }
    // Remove only the original intent inode. If a same-name replacement won
    // the race, leave that foreign intent visible and let startup ignore it;
    // the exact handle-bound link remains recoverable under `bound_name`.
    let intent_observed = match open_child_nofollow(parent, intent_name)
        .and_then(|candidate| opened_file_info(&candidate))
    {
        Ok(observed) => observed,
        Err(_) => return Ok(()),
    };
    if deadline.check().is_err() {
        return Ok(());
    }
    if intent_observed.0 == expected_identity && intent_observed.1 > 0 {
        let removed = unsafe { unix_at::unlinkat(parent.as_raw_fd(), intent.as_ptr(), 0) };
        if removed != 0 {
            return Ok(());
        }
    }
    if deadline.check().is_err() {
        return Ok(());
    }
    if sync_parent_directory_with_deadline(parent, deadline).is_err() {
        return Ok(());
    }
    Ok(())
}

#[cfg(windows)]
fn rename_private_intent(
    parent: &File,
    _intent_name: &str,
    bound_name: &str,
    file: &File,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    match windows_rename_relative(file, parent, bound_name, false, deadline) {
        Ok(()) => Ok(()),
        Err(error) if error.temporary_moved() => Ok(()),
        Err(_) => Err(io::Error::other("private temporary identity rename failed")),
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn rename_private_intent(
    _parent: &File,
    _intent_name: &str,
    _bound_name: &str,
    _file: &File,
    _deadline: &OperationDeadline,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private temporary identity rename is unsupported on this target",
    ))
}

#[cfg(windows)]
fn create_child_exclusive(parent: &File, name: &str) -> io::Result<File> {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    #[repr(C)]
    struct NtUnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct NtObjectAttributes {
        length: u32,
        root_directory: *mut c_void,
        object_name: *mut NtUnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct NtIoStatusBlock {
        status: i32,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut *mut c_void,
            desired_access: u32,
            object_attributes: *const NtObjectAttributes,
            io_status_block: *mut NtIoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    let utf16 = name.encode_utf16().collect::<Vec<_>>();
    let byte_len = utf16
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "temporary name is too long"))?;
    let mut unicode = NtUnicodeString {
        length: byte_len,
        maximum_length: byte_len,
        buffer: utf16.as_ptr().cast_mut(),
    };
    let attributes = NtObjectAttributes {
        length: std::mem::size_of::<NtObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &mut unicode,
        attributes: 0x0000_1040, // OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut status_block = NtIoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            0x001F_01FF, // FILE_ALL_ACCESS
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0x0000_0080, // FILE_ATTRIBUTE_NORMAL
            0x0000_0005, // share read/delete; deny post-validation writers
            0x0000_0002, // FILE_CREATE
            0x0020_0060, // synchronous, non-directory, open-reparse-point
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(if code == 0 {
            31
        } else {
            code as i32
        }));
    }
    if handle.is_null() {
        return Err(io::Error::other(
            "Windows returned an empty temporary handle",
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[allow(dead_code)]
enum AtomicReplaceError {
    Conflict {
        temporary_moved: bool,
    },
    /// The destination state is no longer reversible by a proven inode
    /// binding. Keep the private residue in place and surface an explicit
    /// cleanup failure rather than retaining an attacker-controlled name or
    /// identity for future deletion.
    Uncertain {
        temporary_moved: bool,
        destination_committed: bool,
        name: Option<String>,
        identity: Option<FileIdentity>,
    },
    Tombstone {
        name: String,
        identity: FileIdentity,
        temporary_moved: bool,
        destination_committed: bool,
    },
    Io {
        error: io::Error,
        temporary_moved: bool,
        destination_committed: bool,
    },
}

impl AtomicReplaceError {
    fn conflict(temporary_moved: bool) -> Self {
        Self::Conflict { temporary_moved }
    }

    fn io(error: io::Error, temporary_moved: bool, destination_committed: bool) -> Self {
        Self::Io {
            error,
            temporary_moved,
            destination_committed,
        }
    }

    fn tombstone(
        name: String,
        identity: FileIdentity,
        temporary_moved: bool,
        destination_committed: bool,
    ) -> Self {
        Self::Tombstone {
            name,
            identity,
            temporary_moved,
            destination_committed,
        }
    }

    fn uncertain(temporary_moved: bool, destination_committed: bool) -> Self {
        Self::Uncertain {
            temporary_moved,
            destination_committed,
            name: None,
            identity: None,
        }
    }

    fn uncertain_tombstone(
        name: String,
        identity: FileIdentity,
        temporary_moved: bool,
        destination_committed: bool,
    ) -> Self {
        Self::Uncertain {
            temporary_moved,
            destination_committed,
            name: Some(name),
            identity: Some(identity),
        }
    }

    fn temporary_moved(&self) -> bool {
        match self {
            Self::Conflict { temporary_moved }
            | Self::Uncertain {
                temporary_moved, ..
            }
            | Self::Io {
                temporary_moved, ..
            }
            | Self::Tombstone {
                temporary_moved, ..
            } => *temporary_moved,
        }
    }

    fn destination_committed(&self) -> bool {
        match self {
            Self::Conflict { .. } => false,
            Self::Uncertain {
                destination_committed,
                ..
            }
            | Self::Io {
                destination_committed,
                ..
            }
            | Self::Tombstone {
                destination_committed,
                ..
            } => *destination_committed,
        }
    }

    fn with_temporary_moved(self, temporary_moved: bool) -> Self {
        match self {
            Self::Conflict { .. } => Self::Conflict { temporary_moved },
            Self::Uncertain {
                destination_committed,
                name,
                identity,
                ..
            } => Self::Uncertain {
                temporary_moved,
                destination_committed,
                name,
                identity,
            },
            Self::Io {
                error,
                destination_committed,
                ..
            } => Self::Io {
                error,
                temporary_moved,
                destination_committed,
            },
            Self::Tombstone {
                name,
                identity,
                destination_committed,
                ..
            } => Self::Tombstone {
                name,
                identity,
                temporary_moved,
                destination_committed,
            },
        }
    }
}

fn ensure_mutation_capability(operation: &'static str) -> Result<(), FileServiceError> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        let _ = operation;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(FileServiceError::Unsupported { operation })
    }
}

#[cfg(any(unix, windows))]
fn validate_temporary_path(
    parent: &File,
    temporary: &File,
    temporary_name: &str,
    new_sha256: [u8; 32],
    deadline: &OperationDeadline,
) -> Result<FileIdentity, AtomicReplaceError> {
    let check = || {
        deadline.check().map_err(|_| {
            AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            )
        })
    };
    check()?;
    let (held_identity, held_links) =
        opened_file_info(temporary).map_err(|error| AtomicReplaceError::io(error, false, false))?;
    check()?;
    let held_revision = revision_from_opened_file_with_deadline(temporary, deadline)
        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
    if held_links != 1 || held_revision.sha256 != Some(new_sha256) {
        return Err(AtomicReplaceError::conflict(false));
    }
    check()?;
    let named = open_child_nofollow(parent, temporary_name)
        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
    check()?;
    let (named_identity, named_links) =
        opened_file_info(&named).map_err(|error| AtomicReplaceError::io(error, false, false))?;
    check()?;
    let named_revision = revision_from_opened_file_with_deadline(&named, deadline)
        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
    if named_identity != held_identity
        || named_links != 1
        || named_revision.sha256 != Some(new_sha256)
    {
        return Err(AtomicReplaceError::conflict(false));
    }
    check()?;
    Ok(held_identity)
}

#[cfg(any(unix, windows))]
fn observe_named_revision(
    parent: &File,
    name: &str,
    deadline: &OperationDeadline,
) -> io::Result<(FileRevision, u64)> {
    check_deadline_io(deadline)?;
    let file = open_child_nofollow(parent, name)?;
    check_deadline_io(deadline)?;
    let link_count = opened_file_info(&file)?.1;
    check_deadline_io(deadline)?;
    let revision = revision_from_opened_file_with_deadline(&file, deadline)?;
    check_deadline_io(deadline)?;
    Ok((revision, link_count))
}

#[cfg(target_os = "linux")]
fn rollback_exchange(
    parent: &File,
    temporary_name: &str,
    destination_name: &str,
    temporary_identity: FileIdentity,
    expected_old_identity: FileIdentity,
    deadline: &OperationDeadline,
) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    const RENAME_EXCHANGE: u32 = 2;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let destination = open_child_nofollow(parent, destination_name)?;
    check_deadline_io(deadline)?;
    let (destination_identity, _) = opened_file_info(&destination)?;
    check_deadline_io(deadline)?;
    if destination_identity != temporary_identity {
        return Ok(false);
    }
    check_deadline_io(deadline)?;
    let old = open_child_nofollow(parent, temporary_name)?;
    check_deadline_io(deadline)?;
    let (old_identity, _) = opened_file_info(&old)?;
    check_deadline_io(deadline)?;
    if old_identity != expected_old_identity {
        return Ok(false);
    }
    let temporary = CString::new(temporary_name).map_err(io::Error::other)?;
    let destination = CString::new(destination_name).map_err(io::Error::other)?;
    check_deadline_io(deadline)?;
    let result = unsafe {
        unix_at::renameat2(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    check_deadline_io(deadline)?;
    if result == 0 {
        check_deadline_io(deadline)?;
        Ok(true)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn unlink_exchange_temporary_aliases(
    parent: &File,
    expected_identity: FileIdentity,
    expected_parent_identity: FileIdentity,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    check_deadline_io(deadline)?;
    let entries = read_directory_from_handle(parent, Path::new("."))?;
    for entry in entries {
        check_deadline_io(deadline)?;
        let Some(name) = entry.to_str() else {
            continue;
        };
        let Some(binding) = parse_temporary_binding(name) else {
            continue;
        };
        if binding.parent_identity != Some(expected_parent_identity)
            || binding.expected_target_identity != Some(expected_identity)
        {
            continue;
        }
        let observed =
            match open_child_nofollow(parent, name).and_then(|file| opened_file_info(&file)) {
                Ok(observed) => observed,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
        if observed.0 != expected_identity || observed.1 < 2 {
            continue;
        }
        unlink_exact_private_link_if_identity(parent, name, expected_identity, deadline)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn exchange_tombstone(
    parent: &File,
    source_name: &str,
    expected_identity: FileIdentity,
    held_old: Option<&File>,
    deadline: &OperationDeadline,
    preferred_name: Option<&str>,
    accounting: &Arc<CleanupLedger>,
) -> AtomicReplaceError {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    let uncertain_after_exchange = || {
        preferred_name.map_or_else(
            || AtomicReplaceError::uncertain(true, true),
            |name| AtomicReplaceError::tombstone(name.to_string(), expected_identity, true, true),
        )
    };
    if deadline.check().is_err() {
        return uncertain_after_exchange();
    }

    let parent_identity = match opened_file_info(parent) {
        Ok((identity, _)) => identity,
        Err(_) => return uncertain_after_exchange(),
    };
    // Only move the entry when the private source name still denotes the
    // expected inode. If an external writer replaced that name, leave it in
    // place; recording the old private name as uncertain is safer than moving
    // an attacker entry into a recovery name.
    let source_is_expected = match open_child_nofollow(parent, source_name)
        .and_then(|file| opened_file_info(&file).map(|info| (file, info)))
    {
        Ok((_, (identity, links))) => identity == expected_identity && links > 0,
        Err(_) => false,
    };
    if deadline.check().is_err() {
        return uncertain_after_exchange();
    }
    if let Some(preferred_name) = preferred_name {
        let preferred_is_expected = open_child_nofollow(parent, preferred_name)
            .and_then(|file| opened_file_info(&file))
            .is_ok_and(|(identity, links)| identity == expected_identity && links > 0);
        if preferred_is_expected {
            // The pre-exchange anchor is already the exact, identity-bound
            // recovery slot. Remove only the old inode's extra temporary
            // link; never create a second tombstone or ledger slot.
            if source_is_expected {
                if let Err(error) = unlink_exact_private_link_if_identity(
                    parent,
                    source_name,
                    expected_identity,
                    deadline,
                ) {
                    // Keep the anchor visible. Recovery scans the bound
                    // temporary alias and retries this exact unlink later;
                    // never discard the failure and strand a second link.
                    let _ = error;
                    return AtomicReplaceError::tombstone(
                        preferred_name.to_string(),
                        expected_identity,
                        true,
                        true,
                    );
                }
            }
            let _ = accounting;
            return AtomicReplaceError::tombstone(
                preferred_name.to_string(),
                expected_identity,
                true,
                true,
            );
        }
    }
    if !source_is_expected {
        // If the old name was already replaced or removed, try to resurrect
        // the held inode through AT_EMPTY_PATH. This is descriptor-bound and
        // does not touch the attacker's replacement name.
        if let Some(held_old) = held_old {
            if let Ok(Some(name)) =
                link_handle_to_tombstone(parent, held_old, expected_identity, deadline, None)
            {
                return AtomicReplaceError::tombstone(name, expected_identity, true, true);
            }
        }
        return uncertain_after_exchange();
    }
    let tombstone_name = match new_tombstone_name(parent_identity, expected_identity) {
        Ok(name) => name,
        // The exchange has already committed by the time this helper is
        // called. There is no safe pathname to put in a durable record when
        // name generation itself fails, so surface an explicit uncertain
        // state rather than returning a post-commit I/O error that callers
        // could accidentally treat as ordinary rollback.
        Err(_) => return uncertain_after_exchange(),
    };
    let source = match CString::new(source_name) {
        Ok(source) => source,
        Err(_) => return uncertain_after_exchange(),
    };
    let tombstone = match CString::new(tombstone_name.as_str()) {
        Ok(tombstone) => tombstone,
        Err(_) => return uncertain_after_exchange(),
    };
    const RENAME_NOREPLACE: u32 = 1;
    if deadline.check().is_err() {
        return uncertain_after_exchange();
    }
    let moved = unsafe {
        unix_at::renameat2(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            tombstone.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if deadline.check().is_err() {
        return uncertain_after_exchange();
    }
    if moved == 0 {
        match observe_named_revision(parent, tombstone_name.as_str(), deadline) {
            Ok((revision, links))
                if links > 0 && revision.fingerprint.identity == expected_identity =>
            {
                AtomicReplaceError::tombstone(tombstone_name, expected_identity, true, true)
            }
            Ok(_) | Err(_) => uncertain_after_exchange(),
        }
    } else {
        if let Some(held_old) = held_old {
            if let Ok(Some(name)) =
                link_handle_to_tombstone(parent, held_old, expected_identity, deadline, None)
            {
                return AtomicReplaceError::tombstone(name, expected_identity, true, true);
            }
        }
        uncertain_after_exchange()
    }
}

#[cfg(target_os = "linux")]
fn link_handle_to_tombstone(
    parent: &File,
    file: &File,
    identity: FileIdentity,
    deadline: &OperationDeadline,
    source_name: Option<&str>,
) -> io::Result<Option<String>> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let parent_identity = opened_file_info(parent)?.0;
    let name = new_tombstone_name(parent_identity, identity)?;
    let empty = CString::new("").expect("empty C string");
    let name_c = CString::new(name.as_str()).map_err(io::Error::other)?;
    const AT_EMPTY_PATH: i32 = 0x1000;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let result = unsafe {
        unix_at::linkat(
            file.as_raw_fd(),
            empty.as_ptr(),
            parent.as_raw_fd(),
            name_c.as_ptr(),
            AT_EMPTY_PATH,
        )
    };
    let mut linked_by_handle = result == 0;
    let mut linked_by_path = false;
    if result != 0
        && source_name.is_some()
        && matches!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        )
    {
        let source =
            CString::new(source_name.expect("source name is present")).map_err(io::Error::other)?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let path_result = unsafe {
            unix_at::linkat(
                parent.as_raw_fd(),
                source.as_ptr(),
                parent.as_raw_fd(),
                name_c.as_ptr(),
                0,
            )
        };
        linked_by_handle = path_result == 0;
        linked_by_path = linked_by_handle;
    }
    if linked_by_handle || linked_by_path {
        if linked_by_path {
            if deadline.check().is_err() {
                return Ok(Some(name));
            }
            let observed =
                open_child_nofollow(parent, name).and_then(|child| opened_file_info(&child));
            if !matches!(observed, Ok((observed_identity, _)) if observed_identity == identity) {
                return Ok(None);
            }
        }
        // The hard link is already committed. Return its exact generated
        // identity-bound name even when the post-effect budget expires so the
        // caller can retain it as visible uncertainty.
        let _ = check_deadline_io(deadline);
        Ok(Some(name))
    } else {
        check_deadline_io(deadline)?;
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
        ) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

fn atomic_replace(
    parent: &File,
    temporary: &File,
    temporary_name: &str,
    destination_name: &str,
    replacing: bool,
    expected_revision: Option<&FileRevision>,
    new_sha256: [u8; 32],
    accounting: &Arc<CleanupLedger>,
    deadline: &OperationDeadline,
) -> Result<(), AtomicReplaceError> {
    let _ = accounting;
    let deadline_error = || {
        AtomicReplaceError::io(
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            ),
            false,
            false,
        )
    };
    let _committed_deadline_error = || AtomicReplaceError::Io {
        error: io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded after rename",
        ),
        temporary_moved: true,
        destination_committed: true,
    };
    if deadline.check().is_err() {
        return Err(deadline_error());
    }
    let parent_identity = opened_file_info(parent)
        .map(|(identity, _)| identity)
        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
    if deadline.check().is_err() {
        return Err(deadline_error());
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "linux")]
        let temporary_identity =
            validate_temporary_path(parent, temporary, temporary_name, new_sha256, deadline)?;
        if deadline.check().is_err() {
            return Err(deadline_error());
        }
        let temporary = CString::new(temporary_name)
            .map_err(io::Error::other)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        let destination = CString::new(destination_name)
            .map_err(io::Error::other)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        if !replacing {
            if deadline.check().is_err() {
                return Err(deadline_error());
            }
            #[cfg(target_os = "linux")]
            {
                const RENAME_NOREPLACE: u32 = 1;
                let result = unsafe {
                    unix_at::renameat2(
                        parent.as_raw_fd(),
                        temporary.as_ptr(),
                        parent.as_raw_fd(),
                        destination.as_ptr(),
                        RENAME_NOREPLACE,
                    )
                };
                if result != 0 {
                    if deadline.check().is_err() {
                        return Err(deadline_error());
                    }
                    let error = io::Error::last_os_error();
                    return if error.kind() == io::ErrorKind::AlreadyExists
                        || error.kind() == io::ErrorKind::NotFound
                    {
                        Err(AtomicReplaceError::conflict(false))
                    } else {
                        Err(AtomicReplaceError::io(error, false, false))
                    };
                }
                if deadline.check().is_err() {
                    return Err(_committed_deadline_error());
                }
                // Validate the installed inode through the parent handle. If
                // validation fails, move it back to the still-private source
                // name without replacing a concurrent entry. If that exact
                // rollback is unavailable, retain an identity-bound record.
                match observe_named_revision(parent, destination_name, deadline) {
                    Ok((revision, link_count))
                        if link_count == 1
                            && revision.fingerprint.identity == temporary_identity
                            && revision.sha256 == Some(new_sha256) =>
                    {
                        return Ok(());
                    }
                    Ok(_) | Err(_) => {
                        if deadline.check().is_err() {
                            return Err(deadline_error());
                        }
                        let rollback = unsafe {
                            unix_at::renameat2(
                                parent.as_raw_fd(),
                                destination.as_ptr(),
                                parent.as_raw_fd(),
                                temporary.as_ptr(),
                                RENAME_NOREPLACE,
                            )
                        };
                        if deadline.check().is_err() {
                            return Err(deadline_error());
                        }
                        if rollback == 0 {
                            return Err(AtomicReplaceError::conflict(false));
                        }
                        // A failed rollback leaves the destination name
                        // potentially under another writer's control. Never
                        // move that name into a recovery tombstone until the
                        // held temporary identity is proven there again.
                        match observe_named_revision(parent, destination_name, deadline) {
                            Ok((revision, link_count))
                                if link_count == 1
                                    && revision.fingerprint.identity == temporary_identity
                                    && revision.sha256 == Some(new_sha256) => {}
                            Ok(_) | Err(_) => {
                                return Err(AtomicReplaceError::uncertain(true, true));
                            }
                        }
                        let tombstone_name =
                            new_tombstone_name(parent_identity, temporary_identity)
                                .map_err(|_| AtomicReplaceError::uncertain(true, true))?;
                        let tombstone = CString::new(tombstone_name.as_str())
                            .map_err(io::Error::other)
                            .map_err(|_| AtomicReplaceError::uncertain(true, true))?;
                        if deadline.check().is_err() {
                            return Err(AtomicReplaceError::uncertain(true, true));
                        }
                        let moved = unsafe {
                            unix_at::renameat2(
                                parent.as_raw_fd(),
                                destination.as_ptr(),
                                parent.as_raw_fd(),
                                tombstone.as_ptr(),
                                RENAME_NOREPLACE,
                            )
                        };
                        if moved == 0 {
                            if deadline.check().is_err() {
                                return Err(AtomicReplaceError::tombstone(
                                    tombstone_name,
                                    temporary_identity,
                                    true,
                                    true,
                                ));
                            }
                            return match observe_named_revision(
                                parent,
                                tombstone_name.as_str(),
                                deadline,
                            ) {
                                Ok((revision, link_count))
                                    if link_count == 1
                                        && revision.fingerprint.identity == temporary_identity
                                        && revision.sha256 == Some(new_sha256) =>
                                {
                                    Err(AtomicReplaceError::tombstone(
                                        tombstone_name,
                                        temporary_identity,
                                        true,
                                        true,
                                    ))
                                }
                                Ok(_) | Err(_) => Err(AtomicReplaceError::tombstone(
                                    tombstone_name,
                                    temporary_identity,
                                    true,
                                    true,
                                )),
                            };
                        }
                        if deadline.check().is_err() {
                            return Err(deadline_error());
                        }
                        return Err(AtomicReplaceError::uncertain(true, true));
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (
                    parent,
                    temporary,
                    temporary_name,
                    destination_name,
                    replacing,
                    expected_revision,
                    new_sha256,
                );
                return Err(AtomicReplaceError::io(
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "handle-safe replacement is unsupported on this Unix target",
                    ),
                    false,
                    false,
                ));
            }
        }
        #[cfg(target_os = "linux")]
        {
            const RENAME_EXCHANGE: u32 = 2;
            let old_handle = expected_revision
                .ok_or_else(|| AtomicReplaceError::conflict(false))
                .and_then(|expected| {
                    if deadline.check().is_err() {
                        return Err(deadline_error());
                    }
                    let old = open_child_nofollow(parent, destination_name)
                        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
                    let (identity, links) = opened_file_info(&old)
                        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
                    let revision = revision_from_opened_file_with_deadline(&old, deadline)
                        .map_err(|error| AtomicReplaceError::io(error, false, false))?;
                    if links != 1
                        || identity != expected.fingerprint.identity
                        || revision != *expected
                    {
                        return Err(AtomicReplaceError::conflict(false));
                    }
                    Ok(old)
                })?;
            let old_identity = expected_revision
                .map(|revision| revision.fingerprint.identity)
                .ok_or_else(|| AtomicReplaceError::conflict(false))?;
            // Keep one identity-bound tombstone link for the old inode before
            // exchange. If the post-exchange budget expires, the temporary
            // pathname may now carry the old inode, but this generated
            // tombstone remains the durable restart anchor. Restart accepts
            // only the inode identity encoded in each generated name.
            let old_tombstone_name = match link_handle_to_tombstone(
                parent,
                &old_handle,
                old_identity,
                deadline,
                Some(destination_name),
            ) {
                Ok(Some(name)) => name,
                Ok(None) => return Err(AtomicReplaceError::uncertain(false, false)),
                Err(error) => return Err(AtomicReplaceError::io(error, false, false)),
            };
            if sync_parent_directory_with_deadline(parent, deadline).is_err() {
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    false,
                    false,
                ));
            }
            if deadline.check().is_err() {
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    false,
                    false,
                ));
            }
            #[cfg(test)]
            test_pause(TEST_PAUSE_BEFORE_EXCHANGE);
            let result = unsafe {
                unix_at::renameat2(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    destination.as_ptr(),
                    RENAME_EXCHANGE,
                )
            };
            if result != 0 {
                if deadline.check().is_err() {
                    return Err(AtomicReplaceError::tombstone(
                        old_tombstone_name,
                        old_identity,
                        false,
                        false,
                    ));
                }
                let error = io::Error::last_os_error();
                let _ = error;
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    false,
                    false,
                ));
            }
            if deadline.check().is_err() {
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    true,
                    true,
                ));
            }
            #[cfg(test)]
            test_pause(TEST_PAUSE_AFTER_EXCHANGE);
            let old_observed = match observe_named_revision(parent, temporary_name, deadline) {
                Ok(observed) => observed,
                Err(error) => {
                    let failure = AtomicReplaceError::io(error, true, false);
                    return match rollback_exchange(
                        parent,
                        temporary_name,
                        destination_name,
                        temporary_identity,
                        old_identity,
                        deadline,
                    ) {
                        Ok(true) => Err(failure.with_temporary_moved(false)),
                        Ok(false) | Err(_) => Err(exchange_tombstone(
                            parent,
                            temporary_name,
                            old_identity,
                            Some(&old_handle),
                            deadline,
                            Some(old_tombstone_name.as_str()),
                            accounting,
                        )),
                    };
                }
            };
            if deadline.check().is_err() {
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    true,
                    true,
                ));
            }
            // The pre-exchange tombstone link intentionally gives the old
            // inode a second link while it is still held at the temporary
            // pathname. Only zero links or an identity/revision mismatch is
            // unsafe here.
            if old_observed.1 == 0
                || expected_revision.is_none_or(|expected| expected != &old_observed.0)
            {
                let failure = AtomicReplaceError::conflict(true);
                return match rollback_exchange(
                    parent,
                    temporary_name,
                    destination_name,
                    temporary_identity,
                    old_identity,
                    deadline,
                ) {
                    Ok(true) => Err(failure.with_temporary_moved(false)),
                    Ok(false) | Err(_) => Err(exchange_tombstone(
                        parent,
                        temporary_name,
                        old_identity,
                        Some(&old_handle),
                        deadline,
                        Some(old_tombstone_name.as_str()),
                        accounting,
                    )),
                };
            }
            let new_observed = match observe_named_revision(parent, destination_name, deadline) {
                Ok(observed) => observed,
                Err(error) => {
                    let failure = AtomicReplaceError::io(error, true, false);
                    return match rollback_exchange(
                        parent,
                        temporary_name,
                        destination_name,
                        temporary_identity,
                        old_identity,
                        deadline,
                    ) {
                        Ok(true) => Err(failure.with_temporary_moved(false)),
                        Ok(false) | Err(_) => Err(exchange_tombstone(
                            parent,
                            temporary_name,
                            old_identity,
                            Some(&old_handle),
                            deadline,
                            Some(old_tombstone_name.as_str()),
                            accounting,
                        )),
                    };
                }
            };
            if deadline.check().is_err() {
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    true,
                    true,
                ));
            }
            if new_observed.1 != 1
                || new_observed.0.fingerprint.identity != temporary_identity
                || new_observed.0.sha256 != Some(new_sha256)
            {
                let failure = AtomicReplaceError::conflict(true);
                return match rollback_exchange(
                    parent,
                    temporary_name,
                    destination_name,
                    temporary_identity,
                    old_identity,
                    deadline,
                ) {
                    Ok(true) => Err(failure.with_temporary_moved(false)),
                    Ok(false) | Err(_) => Err(exchange_tombstone(
                        parent,
                        temporary_name,
                        old_identity,
                        Some(&old_handle),
                        deadline,
                        Some(old_tombstone_name.as_str()),
                        accounting,
                    )),
                };
            }
            #[cfg(test)]
            test_pause(TEST_PAUSE_BEFORE_UNLINK);
            if unlink_private_name_if_identity(
                parent,
                temporary_name,
                old_identity,
                accounting,
                deadline,
            )
            .is_err()
            {
                return Err(exchange_tombstone(
                    parent,
                    temporary_name,
                    old_identity,
                    Some(&old_handle),
                    deadline,
                    Some(old_tombstone_name.as_str()),
                    accounting,
                ));
            }
            if unlink_private_name_if_identity(
                parent,
                old_tombstone_name.as_str(),
                old_identity,
                accounting,
                deadline,
            )
            .is_err()
            {
                return Err(AtomicReplaceError::tombstone(
                    old_tombstone_name,
                    old_identity,
                    true,
                    true,
                ));
            }
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (
                parent,
                temporary,
                temporary_name,
                destination_name,
                replacing,
                expected_revision,
                new_sha256,
            );
            Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "handle-safe replacement is unsupported on this Unix target",
                ),
                false,
                false,
            ))
        }
    }
    #[cfg(windows)]
    {
        let temporary_identity =
            validate_temporary_path(parent, temporary, temporary_name, new_sha256, deadline)?;
        if deadline.check().is_err() {
            return Err(deadline_error());
        }
        if replacing {
            if deadline.check().is_err() {
                return Err(deadline_error());
            }
            let target =
                open_child_nofollow_for_cas(parent, destination_name).map_err(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        AtomicReplaceError::conflict(false)
                    } else {
                        AtomicReplaceError::io(error, false, false)
                    }
                })?;
            let (identity, link_count) = opened_file_info(&target)
                .map_err(|error| AtomicReplaceError::io(error, false, false))?;
            let observed = revision_from_opened_file_with_deadline(&target, deadline)
                .map_err(|error| AtomicReplaceError::io(error, false, false))?;
            if link_count != 1
                || expected_revision != Some(&observed)
                || Some(identity) != expected_revision.map(|revision| revision.fingerprint.identity)
            {
                return Err(AtomicReplaceError::conflict(false));
            }
            if deadline.check().is_err() {
                return Err(deadline_error());
            }
            #[cfg(test)]
            test_pause(TEST_PAUSE_BEFORE_OLD_DETACH);
            // Recheck the held temporary immediately before any replacement
            // rename. The create handle denies a later write-share, and this
            // final name/identity/hash check also rejects an attacker that
            // replaced the private temporary pathname during the pause.
            if validate_temporary_path(parent, temporary, temporary_name, new_sha256, deadline)?
                != temporary_identity
            {
                return Err(AtomicReplaceError::conflict(false));
            }
            // Detach the exact validated handle first. The destination is
            // empty before the temporary is installed, so a same-name writer
            // can only make the no-replace second step fail.
            let tombstone_name = new_tombstone_name(parent_identity, identity)
                .map_err(|error| AtomicReplaceError::io(error, false, false))?;
            match windows_rename_relative(&target, parent, tombstone_name.as_str(), false, deadline)
            {
                Ok(()) => {}
                Err(error) if error.temporary_moved() => {
                    return Err(AtomicReplaceError::tombstone(
                        tombstone_name,
                        identity,
                        false,
                        false,
                    ));
                }
                Err(error) => return Err(error),
            }
            #[cfg(test)]
            test_pause(TEST_PAUSE_AFTER_OLD_DETACH);
            if deadline.check().is_err() {
                return Err(AtomicReplaceError::tombstone(
                    tombstone_name,
                    identity,
                    false,
                    false,
                ));
            }
            match windows_rename_relative(temporary, parent, destination_name, false, deadline) {
                Ok(()) => {
                    #[cfg(test)]
                    test_pause(TEST_PAUSE_AFTER_INSTALL);
                    let installed = open_child_nofollow(parent, destination_name)
                        .and_then(|file| {
                            let info = opened_file_info(&file)?;
                            let revision =
                                revision_from_opened_file_with_deadline(&file, deadline)?;
                            Ok((info, revision))
                        })
                        .map_err(|_error| {
                            // The old inode is already detached under this
                            // generated name. Any post-install observation
                            // failure must carry that existing tombstone to
                            // the caller before returning.
                            AtomicReplaceError::tombstone(
                                tombstone_name.clone(),
                                identity,
                                true,
                                true,
                            )
                        })?;
                    if installed.0 .0 != temporary_identity
                        || installed.0 .1 != 1
                        || installed.1.sha256 != Some(new_sha256)
                    {
                        return Err(AtomicReplaceError::tombstone(
                            tombstone_name,
                            identity,
                            true,
                            true,
                        ));
                    }
                    #[cfg(test)]
                    if TEST_FORCE_OLD_DELETE_FAILURE.load(Ordering::Acquire) {
                        return Err(AtomicReplaceError::tombstone(
                            tombstone_name,
                            identity,
                            true,
                            true,
                        ));
                    }
                    let delete_result = delete_opened_file(&target);
                    if deadline.check().is_err() {
                        // The destination install already committed. Retain
                        // the old-inode tombstone by identity so the caller
                        // can publish it before returning the deadline.
                        return Err(AtomicReplaceError::tombstone(
                            tombstone_name,
                            identity,
                            true,
                            true,
                        ));
                    }
                    if let Err(_error) = delete_result {
                        return Err(AtomicReplaceError::tombstone(
                            tombstone_name,
                            identity,
                            true,
                            true,
                        ));
                    }
                    return Ok(());
                }
                Err(error) if error.temporary_moved() => {
                    return Err(AtomicReplaceError::tombstone(
                        tombstone_name,
                        identity,
                        true,
                        true,
                    ));
                }
                Err(error) => {
                    if windows_rename_relative(&target, parent, destination_name, false, deadline)
                        .is_ok()
                    {
                        return Err(error);
                    }
                    return Err(AtomicReplaceError::tombstone(
                        tombstone_name,
                        identity,
                        false,
                        false,
                    ));
                }
            }
        }
        // A missing target is committed with a handle-relative no-replace
        // rename, which atomically rejects a destination created after plan.
        match open_child_nofollow(parent, destination_name) {
            Ok(_) => Err(AtomicReplaceError::conflict(false)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if validate_temporary_path(parent, temporary, temporary_name, new_sha256, deadline)?
                    != temporary_identity
                {
                    return Err(AtomicReplaceError::conflict(false));
                }
                windows_rename_relative(temporary, parent, destination_name, false, deadline)?;
                let installed =
                    match open_child_nofollow(parent, destination_name).and_then(|file| {
                        let info = opened_file_info(&file)?;
                        let revision = revision_from_opened_file_with_deadline(&file, deadline)?;
                        Ok((info, revision))
                    }) {
                        Ok(installed) => installed,
                        Err(_) => {
                            return Err(tombstone_installed_temporary(
                                temporary,
                                parent,
                                temporary_identity,
                                deadline,
                            ));
                        }
                    };
                if installed.0 .0 != temporary_identity
                    || installed.0 .1 != 1
                    || installed.1.sha256 != Some(new_sha256)
                {
                    return Err(tombstone_installed_temporary(
                        temporary,
                        parent,
                        temporary_identity,
                        deadline,
                    ));
                }
                Ok(())
            }
            Err(error) => Err(AtomicReplaceError::io(error, false, false)),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            parent,
            temporary,
            temporary_name,
            destination_name,
            replacing,
            expected_revision,
            new_sha256,
        );
        Err(AtomicReplaceError::io(
            io::Error::other("atomic replacement is unsupported on this platform"),
            false,
            false,
        ))
    }
}

#[cfg(windows)]
fn tombstone_installed_temporary(
    temporary: &File,
    parent: &File,
    identity: FileIdentity,
    deadline: &OperationDeadline,
) -> AtomicReplaceError {
    if deadline.check().is_err() {
        return AtomicReplaceError::uncertain(true, true);
    }
    let parent_identity = match opened_file_info(parent) {
        Ok((identity, _)) => identity,
        Err(_) => return AtomicReplaceError::uncertain(true, true),
    };
    let tombstone_name = match new_tombstone_name(parent_identity, identity) {
        Ok(name) => name,
        // The destination rename has already committed. Without a generated
        // identity-bound name there is no safe recovery pathname to return.
        Err(_) => return AtomicReplaceError::uncertain(true, true),
    };
    match windows_rename_relative(temporary, parent, tombstone_name.as_str(), false, deadline) {
        Ok(()) => AtomicReplaceError::tombstone(tombstone_name, identity, true, true),
        Err(error) if error.temporary_moved() => {
            AtomicReplaceError::tombstone(tombstone_name, identity, true, true)
        }
        Err(_) => AtomicReplaceError::uncertain(true, true),
    }
}

#[cfg(windows)]
fn windows_rename_relative(
    file: &File,
    parent: &File,
    name: &str,
    replacing: bool,
    deadline: &OperationDeadline,
) -> Result<(), AtomicReplaceError> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    #[repr(C)]
    struct NtIoStatusBlock {
        status: i32,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file_handle: *mut c_void,
            io_status_block: *mut NtIoStatusBlock,
            file_information: *mut c_void,
            length: u32,
            file_information_class: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    if deadline.check().is_err() {
        return Err(AtomicReplaceError::io(
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            ),
            false,
            false,
        ));
    }
    let encoded = name.encode_utf16().collect::<Vec<_>>();
    let header_size = size_of::<FILE_RENAME_INFO>() - size_of::<u16>();
    let total_size = header_size
        .checked_add(encoded.len().checked_mul(size_of::<u16>()).ok_or_else(|| {
            AtomicReplaceError::io(
                io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long"),
                false,
                false,
            )
        })?)
        .ok_or_else(|| {
            AtomicReplaceError::io(
                io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long"),
                false,
                false,
            )
        })?;
    let word_count = total_size
        .checked_add(size_of::<u64>() - 1)
        .ok_or_else(|| {
            AtomicReplaceError::io(
                io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long"),
                false,
                false,
            )
        })?
        / size_of::<u64>();
    let mut storage = vec![0_u64; word_count];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let mut status_block = NtIoStatusBlock {
        status: 0,
        information: 0,
    };
    if deadline.check().is_err() {
        return Err(AtomicReplaceError::io(
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            ),
            false,
            false,
        ));
    }
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = replacing;
        (*info).RootDirectory = HANDLE(parent.as_raw_handle());
        (*info).FileNameLength = (encoded.len() * size_of::<u16>()) as u32;
        std::ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            encoded.len(),
        );
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            info.cast(),
            total_size as u32,
            10, // FileRenameInformation
        )
    };
    let deadline_result = deadline.check();
    if status >= 0 {
        deadline_result.map_err(|_| {
            AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                true,
                true,
            )
        })?;
        Ok(())
    } else {
        if deadline_result.is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        let code = unsafe { RtlNtStatusToDosError(status) };
        if matches!(code, 32 | 80 | 183) {
            Err(AtomicReplaceError::conflict(false))
        } else {
            Err(AtomicReplaceError::io(
                io::Error::from_raw_os_error(if code == 0 { 31 } else { code as i32 }),
                false,
                false,
            ))
        }
    }
}

#[cfg(unix)]
fn unlink_target(parent: &File, name: &str) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let name = CString::new(name).map_err(io::Error::other)?;
    let result = unsafe { unix_at::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn unlink_exact_private_link_if_identity(
    parent: &File,
    name: &str,
    expected_identity: FileIdentity,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    // The pre-exchange anchor keeps one link alive. This helper removes only
    // the other, exact link after two descriptor observations; it deliberately
    // does not quarantine the link into a second ledger slot.
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let observed = open_child_nofollow(parent, name).and_then(|file| opened_file_info(&file))?;
    if observed.0 != expected_identity || observed.1 < 2 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private workspace identity changed",
        ));
    }
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let rebound = open_child_nofollow(parent, name).and_then(|file| opened_file_info(&file))?;
    if rebound.0 != expected_identity || rebound.1 < 2 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private workspace identity changed",
        ));
    }
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let result = unlink_target(parent, name);
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    result
}

#[cfg(target_os = "linux")]
struct CleanupAuthority {
    handle: File,
    identity: FileIdentity,
}

#[cfg(target_os = "linux")]
static CLEANUP_AUTHORITY: OnceLock<CleanupAuthority> = OnceLock::new();

#[cfg(target_os = "linux")]
fn is_private_cleanup_authority(handle: &File) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = handle.metadata()?;
    let mode = metadata.permissions().mode();
    Ok(metadata.is_dir() && mode & 0o700 == 0o700 && mode & 0o077 == 0)
}

#[cfg(target_os = "linux")]
fn cleanup_authority(deadline: &OperationDeadline) -> io::Result<&'static CleanupAuthority> {
    check_deadline_io(deadline)?;
    if let Some(authority) = CLEANUP_AUTHORITY.get() {
        return Ok(authority);
    }

    let mut random = [0_u8; 16];
    fill_random(&mut random).map_err(io::Error::other)?;
    check_deadline_io(deadline)?;
    let suffix = encode_nonce(&random);
    let path = std::env::temp_dir().join(format!(".devmanager-file-cleanup-{suffix}"));
    check_deadline_io(deadline)?;
    fs::create_dir(&path)?;
    if let Err(error) = check_deadline_io(deadline) {
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    // Restrict the held directory to this user. This is deliberately outside
    // every workspace. The private directory is only an exact-identity hold;
    // a same-UID process that bypasses this boundary remains out of scope.
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir(&path);
            return Err(error);
        }
    }
    if let Err(error) = check_deadline_io(deadline) {
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    let handle = match open_nofollow(&path, true, true) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = fs::remove_dir(&path);
            return Err(error);
        }
    };
    if let Err(error) = check_deadline_io(deadline) {
        drop(handle);
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    let identity = match opened_file_info(&handle) {
        Ok((identity, _)) => identity,
        Err(error) => {
            let _ = fs::remove_dir(&path);
            return Err(error);
        }
    };
    if let Err(error) = check_deadline_io(deadline) {
        drop(handle);
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    let candidate = CleanupAuthority { handle, identity };
    match CLEANUP_AUTHORITY.set(candidate) {
        Ok(()) => {}
        Err(candidate) => {
            // Another bounded initializer won the race. The candidate has no
            // retained entries yet, so release its empty private directory.
            drop(candidate.handle);
            let _ = fs::remove_dir(&path);
        }
    }
    check_deadline_io(deadline)?;
    CLEANUP_AUTHORITY
        .get()
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "cleanup authority unavailable"))
}

#[cfg(target_os = "linux")]
fn discover_cleanup_authority(deadline: &OperationDeadline) -> io::Result<()> {
    check_deadline_io(deadline)?;
    let root = std::env::temp_dir();
    check_deadline_io(deadline)?;
    let entries = fs::read_dir(&root)?;
    let mut scanned = 0_usize;
    let mut settled = 0_usize;
    for entry in entries {
        check_deadline_io(deadline)?;
        scanned = scanned.saturating_add(1);
        if scanned > MAX_SEARCH_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "cleanup authority scan exceeded bound",
            ));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".devmanager-file-cleanup-") {
            continue;
        }
        let path = entry.path();
        let authority = match open_nofollow(&path, true, true) {
            Ok(handle) => handle,
            Err(_) => continue,
        };
        check_deadline_io(deadline)?;
        if !is_private_cleanup_authority(&authority)? {
            continue;
        }
        check_deadline_io(deadline)?;
        let children = match read_directory_from_handle(&authority, &path) {
            Ok(children) => children,
            Err(_) => continue,
        };
        for child_name in children {
            check_deadline_io(deadline)?;
            scanned = scanned.saturating_add(1);
            if scanned > MAX_SEARCH_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "cleanup authority scan exceeded bound",
                ));
            }
            let Some(child_name) = child_name.to_str() else {
                continue;
            };
            if !is_private_authority_entry_name(child_name) {
                continue;
            }
            let child = match open_child_nofollow(&authority, child_name) {
                Ok(child) => child,
                Err(_) => continue,
            };
            check_deadline_io(deadline)?;
            let (identity, links) = match opened_file_info(&child) {
                Ok(info) => info,
                Err(_) => continue,
            };
            let Some(binding) = parse_authority_entry_binding(child_name) else {
                continue;
            };
            if identity != binding.identity
                || binding.expected_target_identity != Some(identity)
                || links == 0
            {
                continue;
            }
            if settled >= MAX_TOMBSTONES {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "cleanup authority capacity exceeded",
                ));
            }
            settled += 1;
            check_deadline_io(deadline)?;
            if unlink_target(&authority, child_name).is_ok() {
                sync_parent_directory_with_deadline(&authority, deadline)?;
            }
        }
    }
    check_deadline_io(deadline)?;
    let _ = cleanup_authority(deadline)?;
    Ok(())
}

#[cfg(target_os = "linux")]
enum RestoreCleanupOutcome {
    NotRestored,
    Restored,
    RestoredUncertain(io::Error),
}

#[cfg(target_os = "linux")]
fn restore_cleanup_entry(
    parent: &File,
    name: &str,
    authority: &CleanupAuthority,
    cleanup_name: &str,
    expected_identity: FileIdentity,
    deadline: &OperationDeadline,
) -> io::Result<RestoreCleanupOutcome> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    check_deadline_io(deadline)?;
    let cleanup = open_child_nofollow(&authority.handle, cleanup_name)?;
    check_deadline_io(deadline)?;
    let (identity, links) = opened_file_info(&cleanup)?;
    check_deadline_io(deadline)?;
    let exact = identity == expected_identity && links > 0;
    if !exact {
        return Ok(RestoreCleanupOutcome::NotRestored);
    }
    let source = CString::new(cleanup_name).map_err(io::Error::other)?;
    let destination = CString::new(name).map_err(io::Error::other)?;
    const RENAME_NOREPLACE: u32 = 1;
    check_deadline_io(deadline)?;
    let restored = unsafe {
        unix_at::renameat2(
            authority.handle.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    } == 0;
    if !restored {
        return Ok(RestoreCleanupOutcome::NotRestored);
    }
    // The rename is committed. Any later deadline or fsync observation must
    // retain the restored path as the owned exact residue rather than
    // reporting the old authority pathname.
    if let Err(error) = check_deadline_io(deadline) {
        return Ok(RestoreCleanupOutcome::RestoredUncertain(error));
    }
    if let Err(error) = sync_parent_directory_with_deadline(parent, deadline) {
        return Ok(RestoreCleanupOutcome::RestoredUncertain(error));
    }
    if let Err(error) = check_deadline_io(deadline) {
        return Ok(RestoreCleanupOutcome::RestoredUncertain(error));
    }
    if let Err(error) = sync_parent_directory_with_deadline(&authority.handle, deadline) {
        return Ok(RestoreCleanupOutcome::RestoredUncertain(error));
    }
    if let Err(error) = check_deadline_io(deadline) {
        return Ok(RestoreCleanupOutcome::RestoredUncertain(error));
    }
    Ok(RestoreCleanupOutcome::Restored)
}

#[cfg(target_os = "linux")]
fn restore_or_retain_cleanup_entry(
    original: io::Error,
    parent: &File,
    original_parent_identity: FileIdentity,
    name: &str,
    authority: &CleanupAuthority,
    cleanup_name: &str,
    expected_identity: FileIdentity,
    accounting: &Arc<CleanupLedger>,
    reservation: &mut TombstoneReservation,
    deadline: &OperationDeadline,
) -> io::Error {
    let mut retain_authority_entry = || {
        let Some(binding) = parse_authority_entry_binding(cleanup_name) else {
            return;
        };
        if binding.identity != expected_identity
            || binding.expected_target_identity != Some(expected_identity)
        {
            return;
        }
        let Ok(parent) = authority.handle.try_clone() else {
            return;
        };
        let current_name = cleanup_name.to_string();
        let current_identity = expected_identity;
        persist_cleanup_record(
            accounting,
            reservation,
            TombstoneRecord {
                parent,
                parent_identity: authority.identity,
                expected_parent_identity: binding.parent_identity.unwrap_or(authority.identity),
                name: current_name.clone(),
                identity: current_identity,
                expected_target_identity: binding.expected_target_identity,
                operation_nonce: binding.operation_nonce,
                uncertain: true,
                recovering: false,
            },
        );
    };
    // A failed restore may itself have consumed the operation budget. The
    // post-effect record is inserted without a second blocking operation so
    // the exact authority path remains restart-discoverable before timeout is
    // returned to the caller.
    if let Err(error) = check_deadline_io(deadline) {
        retain_authority_entry();
        return error;
    }

    // Recovery already owns the ledger slot. Hold its record lock across the
    // authority-to-workspace rename so the current path and identity can be
    // published before any post-effect deadline observation. Normal Drop
    // cleanup has no record yet and inserts one only for uncertain restore.
    let mut recovery_records = if reservation.released {
        match accounting.tombstones.lock_until(deadline) {
            Ok(records) => Some(records),
            Err(_) => {
                retain_authority_entry();
                return original;
            }
        }
    } else {
        None
    };
    let mut restore_parent_record = match parent.try_clone() {
        Ok(parent_handle) => Some(parent_handle),
        Err(_) => {
            // Without a retained descriptor for the original parent, a
            // successful authority-to-workspace rename could not be
            // published into the sole ledger record. Keep the exact
            // authority entry in place instead of creating an unowned
            // workspace residue.
            drop(recovery_records);
            retain_authority_entry();
            return original;
        }
    };
    if let Err(error) = check_deadline_io(deadline) {
        drop(recovery_records);
        retain_authority_entry();
        return error;
    }
    let mut publish_restored_record = |records: &mut Vec<TombstoneRecord>| {
        let Some(parent_handle) = restore_parent_record.take() else {
            return false;
        };
        let updated = update_cleanup_record_after_move(
            records,
            authority.identity,
            cleanup_name,
            expected_identity,
            parent_handle,
            original_parent_identity,
            name,
            expected_identity,
        );
        if let Some(newly_uncertain) = updated {
            if newly_uncertain {
                accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
            }
            return true;
        }
        let Some(binding) = cleanup_name_binding(name) else {
            return false;
        };
        let Ok(parent_handle) = parent.try_clone() else {
            return false;
        };
        records.push(TombstoneRecord {
            parent: parent_handle,
            parent_identity: original_parent_identity,
            expected_parent_identity: binding.parent_identity.unwrap_or(original_parent_identity),
            name: name.to_string(),
            identity: expected_identity,
            expected_target_identity: binding.expected_target_identity,
            operation_nonce: binding.operation_nonce,
            uncertain: true,
            recovering: false,
        });
        accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
        true
    };
    match restore_cleanup_entry(
        parent,
        name,
        authority,
        cleanup_name,
        expected_identity,
        deadline,
    ) {
        Ok(RestoreCleanupOutcome::Restored) => {
            // The authority move was only a temporary recovery transfer. If
            // the exact inode was restored, move the same ledger slot back to
            // the original private name before returning the error/result.
            if let Some(records) = recovery_records.as_deref_mut() {
                let _ = publish_restored_record(records);
            }
            drop(recovery_records);
            reservation.release();
            original
        }
        Ok(RestoreCleanupOutcome::RestoredUncertain(error)) => {
            if let Some(records) = recovery_records.as_deref_mut() {
                let _ = publish_restored_record(records);
            } else {
                persist_cleanup_record(
                    accounting,
                    reservation,
                    TombstoneRecord {
                        parent: match parent.try_clone() {
                            Ok(parent) => parent,
                            Err(_) => return error,
                        },
                        parent_identity: original_parent_identity,
                        expected_parent_identity: original_parent_identity,
                        name: name.to_string(),
                        identity: expected_identity,
                        expected_target_identity: cleanup_expected_target(name),
                        operation_nonce: cleanup_operation_nonce(name, expected_identity),
                        uncertain: true,
                        recovering: false,
                    },
                );
            }
            drop(recovery_records);
            error
        }
        Ok(RestoreCleanupOutcome::NotRestored) => {
            drop(recovery_records);
            retain_authority_entry();
            original
        }
        Err(restore_error) => {
            drop(recovery_records);
            retain_authority_entry();
            restore_error
        }
    }
}

#[cfg(target_os = "linux")]
fn cleanup_exact_private_entry(
    parent: &File,
    name: &str,
    expected_identity: FileIdentity,
    accounting: &Arc<CleanupLedger>,
    slot_already_reserved: bool,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    let deadline_error = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    };
    deadline.check().map_err(|_| deadline_error())?;
    let source_parent_identity = opened_file_info(parent)?.0;
    deadline.check().map_err(|_| deadline_error())?;
    let file = open_child_nofollow(parent, name)?;
    deadline.check().map_err(|_| deadline_error())?;
    let observed = opened_file_info(&file)?;
    deadline.check().map_err(|_| deadline_error())?;
    if observed.0 != expected_identity || observed.1 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private workspace identity changed",
        ));
    }
    #[cfg(test)]
    test_pause(TEST_PAUSE_BEFORE_UNLINK);
    deadline.check().map_err(|_| deadline_error())?;
    // Rebind immediately before the move. Linux has no rename-by-fd
    // primitive, so this closes the check/move race as far as the kernel
    // permits; the destination in the held cleanup directory is checked
    // again before any unlink. A mismatch is never deleted.
    let rebound = open_child_nofollow(parent, name)?;
    deadline.check().map_err(|_| deadline_error())?;
    let rebound_info = opened_file_info(&rebound)?;
    deadline.check().map_err(|_| deadline_error())?;
    if rebound_info.0 != expected_identity || rebound_info.1 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private workspace identity changed",
        ));
    }

    let authority = cleanup_authority(deadline)?;
    deadline.check().map_err(|_| deadline_error())?;
    let mut reservation = if slot_already_reserved {
        TombstoneReservation {
            ledger: Arc::clone(accounting),
            // The recovery record already owns this slot. This facade must
            // never release or reserve a second one while transferring the
            // exact residue through the authority.
            released: true,
        }
    } else {
        reserve_cleanup_slot(accounting)
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "cleanup authority is full"))?
    };
    let operation_nonce = cleanup_operation_nonce(name, expected_identity);
    let cleanup_name = format_cleanup_authority_entry_name(
        source_parent_identity,
        expected_identity,
        &operation_nonce,
    );
    // Recovery already owns a ledger slot. Hold that exact ledger entry's
    // lock across the move so the current authority path and inode can be
    // published before any post-effect deadline observation. A normal Drop
    // cleanup has no existing record and persists its reservation on the
    // error path below.
    let mut recovery_records = if slot_already_reserved {
        Some(
            accounting
                .tombstones
                .lock_until(deadline)
                .map_err(|_| deadline_error())?,
        )
    } else {
        None
    };
    let authority_parent = authority.handle.try_clone()?;
    let authority_parent_record = authority_parent.try_clone()?;
    deadline.check().map_err(|_| deadline_error())?;
    if let Err(error) = check_deadline_io(deadline) {
        return Err(error);
    }
    let source = match CString::new(name).map_err(io::Error::other) {
        Ok(source) => source,
        Err(error) => return Err(error),
    };
    let destination = match CString::new(cleanup_name.as_str()).map_err(io::Error::other) {
        Ok(destination) => destination,
        Err(error) => return Err(error),
    };
    const RENAME_NOREPLACE: u32 = 1;
    let moved = unsafe {
        unix_at::renameat2(
            parent.as_raw_fd(),
            source.as_ptr(),
            authority.handle.as_raw_fd(),
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if moved != 0 {
        return Err(io::Error::last_os_error());
    }
    // Publish the exact authority path and inode while the original recovery
    // slot is still owned. This in-place transfer avoids reserving a second
    // slot and remains the sole record if any post-move check expires.
    let recorded_move = if let Some(records) = recovery_records.as_deref_mut() {
        let updated = update_cleanup_record_after_move(
            records,
            source_parent_identity,
            name,
            expected_identity,
            authority_parent_record,
            authority.identity,
            cleanup_name.as_str(),
            expected_identity,
        );
        if let Some(newly_uncertain) = updated {
            if newly_uncertain {
                accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
            }
        }
        updated.is_some()
    } else {
        // A normal Drop guard has no durable record before this move. Its
        // reservation is committed only if a later observation requires
        // retention; recovery is the only path that updates an existing
        // record in-place under the lock held above.
        false
    };
    if slot_already_reserved && !recorded_move {
        // A recovery slot without its expected in-memory record is itself an
        // uncertainty. Reconstitute the one exact record in place rather than
        // reserving a second slot or allowing the authority inode to escape.
        if let Some(records) = recovery_records.as_deref_mut() {
            records.push(TombstoneRecord {
                parent: authority_parent,
                parent_identity: authority.identity,
                expected_parent_identity: source_parent_identity,
                name: cleanup_name.clone(),
                identity: expected_identity,
                expected_target_identity: Some(expected_identity),
                operation_nonce,
                uncertain: true,
                recovering: true,
            });
            accounting.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
        }
    }
    // The move has been published into the existing record. Release the
    // ledger mutex before any restore attempt so a successful rollback can
    // atomically retarget that same record to the original private name.
    drop(recovery_records);
    if let Err(error) = check_deadline_io(deadline) {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name.as_str(),
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    if let Err(error) = sync_parent_directory_with_deadline(parent, deadline) {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name.as_str(),
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    // Make the host-controlled hold durable before attempting unlink. A
    // crash after this point leaves a recoverable exact inode in the held
    // directory rather than an untracked workspace name.
    if let Err(error) = sync_parent_directory_with_deadline(&authority.handle, deadline) {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name.as_str(),
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    if let Err(error) = check_deadline_io(deadline) {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name,
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    let moved_file = match open_child_nofollow(&authority.handle, cleanup_name.as_str()) {
        Ok(file) => file,
        Err(error) => {
            return Err(restore_or_retain_cleanup_entry(
                error,
                parent,
                source_parent_identity,
                name,
                authority,
                cleanup_name.as_str(),
                expected_identity,
                accounting,
                &mut reservation,
                deadline,
            ));
        }
    };
    if let Err(error) = check_deadline_io(deadline) {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name,
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    let moved_info = match opened_file_info(&moved_file) {
        Ok(info) => info,
        Err(error) => {
            return Err(restore_or_retain_cleanup_entry(
                error,
                parent,
                source_parent_identity,
                name,
                authority,
                cleanup_name.as_str(),
                expected_identity,
                accounting,
                &mut reservation,
                deadline,
            ));
        }
    };
    if moved_info.0 != expected_identity || moved_info.1 == 0 {
        // The moved entry is intentionally retained in the host-controlled
        // directory. Never unlink an inode that was not proven to be ours.
        return Err(restore_or_retain_cleanup_entry(
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cleanup authority identity changed",
            ),
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name,
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    if let Err(error) = check_deadline_io(deadline) {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name,
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    let result = unlink_target(&authority.handle, cleanup_name.as_str())
        .and_then(|()| sync_parent_directory_with_deadline(&authority.handle, deadline));
    if let Err(error) = result {
        return Err(restore_or_retain_cleanup_entry(
            error,
            parent,
            source_parent_identity,
            name,
            authority,
            cleanup_name.as_str(),
            expected_identity,
            accounting,
            &mut reservation,
            deadline,
        ));
    }
    reservation.release();
    Ok(())
}

#[cfg(any(unix, windows))]
fn is_private_temporary_name(name: &str) -> bool {
    parse_temporary_binding(name).is_some()
}

#[cfg(any(unix, windows))]
fn is_private_cleanup_name(name: &str) -> bool {
    parse_tombstone_identity(name).is_some()
        || is_private_temporary_name(name)
        || parse_authority_entry_binding(name).is_some()
}

fn is_private_cleanup_component(name: &str) -> bool {
    #[cfg(any(unix, windows))]
    {
        is_private_cleanup_name(name)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = name;
        false
    }
}

fn private_cleanup_not_found() -> FileServiceError {
    FileServiceError::NotFound {
        path: "<path-redacted>".to_string(),
    }
}

#[cfg(any(unix, windows))]
fn is_private_authority_entry_name(name: &str) -> bool {
    parse_authority_entry_binding(name).is_some()
}

#[cfg(target_os = "linux")]
fn format_cleanup_authority_entry_name(
    expected_parent_identity: FileIdentity,
    expected_target_identity: FileIdentity,
    operation_nonce: &[u8; 16],
) -> String {
    format!(
        "{CLEANUP_AUTHORITY_ENTRY_PREFIX}{:016x}-{:016x}-{:016x}-{:016x}-{}.entry",
        expected_parent_identity.volume_or_device,
        expected_parent_identity.file_or_inode,
        expected_target_identity.volume_or_device,
        expected_target_identity.file_or_inode,
        encode_nonce(operation_nonce),
    )
}

fn parse_authority_entry_binding(name: &str) -> Option<CleanupNameBinding> {
    #[cfg(target_os = "linux")]
    {
        let rest = strip_ascii_case_insensitive_prefix(name, CLEANUP_AUTHORITY_ENTRY_PREFIX)?;
        let rest = strip_ascii_case_insensitive_suffix(rest, ".entry")?;
        let parts = rest.split('-').collect::<Vec<_>>();
        if parts.len() != 5 {
            return None;
        }
        let parent_identity = parse_identity(parts[0], parts[1])?;
        let target_identity = parse_identity(parts[2], parts[3])?;
        if identity_is_zero(parent_identity) || identity_is_zero(target_identity) {
            return None;
        }
        let operation_nonce = parse_nonce(parts[4])?;
        Some(CleanupNameBinding {
            parent_identity: Some(parent_identity),
            expected_target_identity: Some(target_identity),
            identity: target_identity,
            operation_nonce,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        None
    }
}

#[cfg(any(unix, windows))]
fn unlink_private_name(parent: &File, name: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = (parent, name);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-safe mutation is unsupported on macOS",
        ));
    }
    if !is_private_cleanup_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "only private workspace cleanup names may be removed",
        ));
    }
    #[cfg(unix)]
    {
        // Linux has no inode-aware unlink-by-descriptor primitive. Callers
        // first bind this private name to the expected identity; if that
        // binding cannot be observed, cleanup fails closed and the durable
        // tombstone remains for a later recovery attempt.
        return unlink_target(parent, name);
    }
    #[cfg(windows)]
    {
        let file = open_child_nofollow_for_cleanup(parent, name)?;
        return delete_opened_file(&file);
    }
}

#[cfg(any(unix, windows))]
fn unlink_private_name_if_identity(
    parent: &File,
    name: &str,
    expected_identity: FileIdentity,
    accounting: &Arc<CleanupLedger>,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    unlink_private_name_if_identity_with_slot(
        parent,
        name,
        expected_identity,
        accounting,
        false,
        deadline,
    )
}

#[cfg(any(unix, windows))]
fn unlink_private_name_if_identity_with_slot(
    parent: &File,
    name: &str,
    expected_identity: FileIdentity,
    accounting: &Arc<CleanupLedger>,
    _slot_already_reserved: bool,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    let _ = accounting;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    #[cfg(target_os = "macos")]
    {
        let _ = (parent, name, expected_identity);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-safe mutation is unsupported on macOS",
        ));
    }
    #[cfg(windows)]
    {
        // Keep the DELETE-capable handle used for both observations and the
        // disposition request. A second pathname lookup after validation
        // could otherwise select a same-name replacement.
        let file = open_child_nofollow_for_cleanup(parent, name)?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let observed = opened_file_info(&file)?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        if observed.0 != expected_identity || observed.1 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "private workspace identity changed",
            ));
        }
        #[cfg(test)]
        test_pause(TEST_PAUSE_BEFORE_UNLINK);
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let observed_again = opened_file_info(&file)?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        if observed_again.0 != expected_identity || observed_again.1 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "private workspace identity changed",
            ));
        }
        let result = delete_opened_file(&file);
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        return result;
    }
    #[cfg(unix)]
    {
        if is_private_authority_entry_name(name) {
            // Linux's process-private cleanup authority already is the
            // quarantine boundary. Re-quarantining an authority entry would
            // recurse into another hidden name and consume a second slot;
            // remove only the exact identity that the durable record names.
            let file = open_child_nofollow(parent, name)?;
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            let (identity, links) = opened_file_info(&file)?;
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            if identity != expected_identity || links == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "cleanup authority identity changed",
                ));
            }
            let rebound = open_child_nofollow(parent, name)?;
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            let (rebound_identity, rebound_links) = opened_file_info(&rebound)?;
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            if rebound_identity != expected_identity || rebound_links == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "cleanup authority identity changed",
                ));
            }
            let result = unlink_target(parent, name);
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            return result;
        }
        #[cfg(target_os = "linux")]
        {
            return cleanup_exact_private_entry(
                parent,
                name,
                expected_identity,
                accounting,
                _slot_already_reserved,
                deadline,
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            let observed =
                open_child_nofollow(parent, name).and_then(|file| opened_file_info(&file))?;
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            if observed.0 != expected_identity || observed.1 == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "private workspace identity changed",
                ));
            }
            #[cfg(test)]
            test_pause(TEST_PAUSE_BEFORE_UNLINK);
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            let observed_again =
                open_child_nofollow(parent, name).and_then(|file| opened_file_info(&file))?;
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            if observed_again.0 != expected_identity || observed_again.1 == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "private workspace identity changed",
                ));
            }
            let result = unlink_private_name(parent, name);
            deadline.check().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                )
            })?;
            result
        }
    }
}

#[cfg(unix)]
fn reserve_tombstone_name(
    parent: &File,
    identity: FileIdentity,
    deadline: &OperationDeadline,
) -> io::Result<String> {
    let parent_identity = opened_file_info(parent)?.0;
    for _ in 0..32 {
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let name = new_tombstone_name(parent_identity, identity)?;
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        let observed = open_child_nofollow(parent, &name);
        deadline.check().map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace operation deadline exceeded",
            )
        })?;
        match observed {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(name),
            Ok(_) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a private tombstone name",
    ))
}

#[cfg(target_os = "linux")]
fn quarantine_private_temporary(
    parent: &File,
    name: &str,
    expected_identity: FileIdentity,
    deadline: &OperationDeadline,
) -> io::Result<String> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let file = open_child_nofollow(parent, name)?;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let (identity, links) = opened_file_info(&file)?;
    if identity != expected_identity || links != 1 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary identity changed before quarantine",
        ));
    }
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let rebound = open_child_nofollow(parent, name)?;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let (rebound_identity, rebound_links) = opened_file_info(&rebound)?;
    if rebound_identity != expected_identity || rebound_links != 1 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary identity changed before quarantine",
        ));
    }
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let tombstone_name = reserve_tombstone_name(parent, expected_identity, deadline)?;
    let source = CString::new(name).map_err(io::Error::other)?;
    let tombstone = CString::new(tombstone_name.as_str()).map_err(io::Error::other)?;
    const RENAME_NOREPLACE: u32 = 1;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    let moved = unsafe {
        unix_at::renameat2(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            tombstone.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if moved != 0 {
        return Err(io::Error::last_os_error());
    }
    // The rename has committed even when the shared operation budget expires
    // immediately afterward. Return the exact generated name so the caller
    // can commit a durable ledger record without touching the filesystem
    // again; never turn the moved tombstone into an anonymous/original-name
    // record.
    if deadline.check().is_err() {
        return Ok(tombstone_name);
    }
    match observe_named_revision(parent, tombstone_name.as_str(), deadline) {
        Ok((revision, link_count))
            if revision.fingerprint.identity == expected_identity && link_count == 1 =>
        {
            if deadline.check().is_err()
                || sync_parent_directory_with_deadline(parent, deadline).is_err()
            {
                return Ok(tombstone_name);
            }
            Ok(tombstone_name)
        }
        Ok(_) | Err(_) => {
            // The rename already committed. Preserve the generated private
            // name even when post-move observation cannot prove the inode;
            // the caller records visible uncertainty against this exact name
            // and restart recovery will refuse any replacement identity.
            Ok(tombstone_name)
        }
    }
}

#[cfg(unix)]
fn delete_unix_if_identity(
    parent: &File,
    name: &str,
    expected_identity: FileIdentity,
    expected_revision: &FileRevision,
    accounting: &Arc<CleanupLedger>,
    deadline: &OperationDeadline,
) -> Result<(), AtomicReplaceError> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    let name_c = CString::new(name)
        .map_err(io::Error::other)
        .map_err(|error| AtomicReplaceError::io(error, false, false))?;

    #[cfg(target_os = "linux")]
    {
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        const RENAME_NOREPLACE: u32 = 1;
        // Hold and re-hash the exact target immediately before detaching it.
        // The pathname is only an input to this descriptor-relative open; all
        // later recovery decisions use this identity and revision, never a
        // replacement observed at the detached name.
        let target_handle = open_child_nofollow(parent, name)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        let (target_identity, target_links) = opened_file_info(&target_handle)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        let target_revision = revision_from_opened_file_with_deadline(&target_handle, deadline)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        if target_identity != expected_identity
            || target_links != 1
            || target_revision != *expected_revision
        {
            return Err(AtomicReplaceError::conflict(false));
        }
        // Reserve an absent, identity-bound private name. The destination is
        // then detached with RENAME_NOREPLACE, so a same-name writer cannot
        // be deleted by this operation and a tombstone placeholder cannot
        // collide with the real target.
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        let tombstone_name = reserve_tombstone_name(parent, expected_identity, deadline)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        #[cfg(test)]
        test_pause(TEST_PAUSE_BEFORE_OLD_DETACH);
        // Rebind the source name immediately before the detach. Linux has no
        // rename-by-descriptor primitive: a held descriptor alone cannot make
        // renameat2(name, ...) select that inode. This last handle-relative
        // identity *and* SHA check narrows the unavoidable kernel window and,
        // critically, rejects the deterministic same-name replacement race
        // before any replacement can be moved into our tombstone.
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        let final_target = open_child_nofollow(parent, name)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        let (final_identity, final_links) = opened_file_info(&final_target)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        let final_revision = revision_from_opened_file_with_deadline(&final_target, deadline)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        if final_identity != expected_identity
            || final_links != 1
            || final_revision != *expected_revision
        {
            return Err(AtomicReplaceError::conflict(false));
        }
        let tombstone_c = CString::new(tombstone_name.as_str())
            .map_err(io::Error::other)
            .map_err(|error| AtomicReplaceError::io(error, false, false))?;
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace operation deadline exceeded",
                ),
                false,
                false,
            ));
        }
        let detach_result = unsafe {
            unix_at::renameat2(
                parent.as_raw_fd(),
                name_c.as_ptr(),
                parent.as_raw_fd(),
                tombstone_c.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if detach_result != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Err(AtomicReplaceError::conflict(false))
            } else {
                Err(AtomicReplaceError::io(error, false, false))
            };
        }
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::uncertain_tombstone(
                tombstone_name.clone(),
                expected_identity,
                false,
                false,
            ));
        }
        let tombstone_observed =
            match observe_named_revision(parent, tombstone_name.as_str(), deadline) {
                Ok(observed) => observed,
                Err(error) => {
                    // The detached pathname was not observed successfully. It
                    // may now be absent or attacker-owned; never move that name
                    // back into the destination on an unproven observation.
                    let _ = error;
                    return Err(AtomicReplaceError::uncertain_tombstone(
                        tombstone_name,
                        expected_identity,
                        false,
                        false,
                    ));
                }
            };
        if tombstone_observed.0.fingerprint.identity != expected_identity
            || tombstone_observed.1 != 1
            || tombstone_observed.0 != *expected_revision
        {
            // Only an unchanged expected inode may be rolled back. If the
            // detached pathname now names another inode, moving it back to
            // the original destination would silently adopt that writer.
            if tombstone_observed.0.fingerprint.identity == expected_identity
                && tombstone_observed.1 == 1
            {
                if deadline.check().is_err() {
                    return Err(AtomicReplaceError::uncertain_tombstone(
                        tombstone_name.clone(),
                        expected_identity,
                        false,
                        false,
                    ));
                }
                let restored = unsafe {
                    unix_at::renameat2(
                        parent.as_raw_fd(),
                        tombstone_c.as_ptr(),
                        parent.as_raw_fd(),
                        name_c.as_ptr(),
                        RENAME_NOREPLACE,
                    )
                };
                if deadline.check().is_err() {
                    return Err(AtomicReplaceError::uncertain_tombstone(
                        tombstone_name.clone(),
                        expected_identity,
                        false,
                        false,
                    ));
                }
                if restored == 0 {
                    return Err(AtomicReplaceError::conflict(false));
                }
            }
            // If a same-name writer won the detach, put that writer back
            // where it came from when the destination is still empty. This
            // never overwrites a concurrent destination and never adopts the
            // substituted identity as a recovery record.
            else if tombstone_observed.0.fingerprint.identity != expected_identity {
                if deadline.check().is_err() {
                    return Err(AtomicReplaceError::uncertain_tombstone(
                        tombstone_name.clone(),
                        expected_identity,
                        false,
                        false,
                    ));
                }
                let restored = unsafe {
                    unix_at::renameat2(
                        parent.as_raw_fd(),
                        tombstone_c.as_ptr(),
                        parent.as_raw_fd(),
                        name_c.as_ptr(),
                        RENAME_NOREPLACE,
                    )
                };
                if deadline.check().is_err() {
                    return Err(AtomicReplaceError::uncertain_tombstone(
                        tombstone_name.clone(),
                        expected_identity,
                        false,
                        false,
                    ));
                }
                if restored == 0 {
                    return Err(AtomicReplaceError::conflict(false));
                }
                // The private name still denotes an unproven replacement, or
                // the replacement could not be restored because another
                // writer occupied the destination. Do not register this name
                // as a tombstone for the expected inode: startup recovery must
                // never even attempt to delete a substituted inode.
                return Err(AtomicReplaceError::uncertain_tombstone(
                    tombstone_name,
                    expected_identity,
                    false,
                    false,
                ));
            }
            // The detached handle proves only `expected_identity`; the
            // observed value may be a same-name attacker inode. Do not record
            // or later delete that substituted identity. Leave the private
            // residue visible and report an uncertain cleanup state.
            return Err(AtomicReplaceError::uncertain_tombstone(
                tombstone_name,
                expected_identity,
                false,
                false,
            ));
        }
        if deadline.check().is_err() {
            return Err(AtomicReplaceError::uncertain_tombstone(
                tombstone_name.clone(),
                expected_identity,
                false,
                false,
            ));
        }
        if let Err(_error) = unlink_private_name_if_identity(
            parent,
            tombstone_name.as_str(),
            expected_identity,
            accounting,
            deadline,
        ) {
            return Err(AtomicReplaceError::tombstone(
                tombstone_name,
                expected_identity,
                false,
                false,
            ));
        }
        return Ok(());
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            name,
            name_c,
            expected_identity,
            expected_revision,
            accounting,
        );
        Err(AtomicReplaceError::io(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "handle-safe delete is unsupported on this Unix target",
            ),
            false,
            false,
        ))
    }
}

#[cfg(windows)]
fn delete_opened_file(file: &File) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|error| io::Error::from_raw_os_error(error.code().0))
}

fn sync_parent_directory(parent: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        parent.sync_all()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::FlushFileBuffers;
        unsafe { FlushFileBuffers(HANDLE(parent.as_raw_handle())) }
            .map_err(|error| io::Error::from_raw_os_error(error.code().0))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = parent;
        Ok(())
    }
}

fn sync_parent_directory_with_deadline(
    parent: &File,
    deadline: &OperationDeadline,
) -> io::Result<()> {
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })?;
    sync_parent_directory(parent)?;
    deadline.check().map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace operation deadline exceeded",
        )
    })
}

fn path_key_text(path: &str) -> String {
    path.to_ascii_lowercase()
}

#[derive(Clone, Copy)]
struct CleanupNameBinding {
    parent_identity: Option<FileIdentity>,
    expected_target_identity: Option<FileIdentity>,
    identity: FileIdentity,
    operation_nonce: [u8; 16],
}

fn identity_is_zero(identity: FileIdentity) -> bool {
    identity.volume_or_device == 0 && identity.file_or_inode == 0
}

fn encode_nonce(nonce: &[u8; 16]) -> String {
    nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn parse_nonce(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut nonce = [0_u8; 16];
    for (index, byte) in nonce.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(nonce)
}

fn parse_identity(volume: &str, inode: &str) -> Option<FileIdentity> {
    Some(FileIdentity {
        volume_or_device: u64::from_str_radix(volume, 16).ok()?,
        file_or_inode: u64::from_str_radix(inode, 16).ok()?,
    })
}

fn new_operation_nonce() -> io::Result<[u8; 16]> {
    let mut nonce = [0_u8; 16];
    fill_random(&mut nonce).map_err(io::Error::other)?;
    Ok(nonce)
}

fn new_tombstone_name(parent_identity: FileIdentity, identity: FileIdentity) -> io::Result<String> {
    let nonce = new_operation_nonce()?;
    Ok(format!(
        "{TOMBSTONE_PREFIX}{:016x}-{:016x}-{:016x}-{:016x}-{}.old",
        parent_identity.volume_or_device,
        parent_identity.file_or_inode,
        identity.volume_or_device,
        identity.file_or_inode,
        encode_nonce(&nonce),
    ))
}

fn parse_tombstone_binding(name: &str) -> Option<CleanupNameBinding> {
    let rest = strip_ascii_case_insensitive_prefix(name, TOMBSTONE_PREFIX)?;
    let parts = rest.split('-').collect::<Vec<_>>();
    if parts.len() != 5 {
        return None;
    }
    let nonce = parse_nonce(strip_ascii_case_insensitive_suffix(parts[4], ".old")?)?;
    let parent_identity = parse_identity(parts[0], parts[1])?;
    let target_identity = parse_identity(parts[2], parts[3])?;
    if identity_is_zero(parent_identity) || identity_is_zero(target_identity) {
        return None;
    }
    Some(CleanupNameBinding {
        parent_identity: Some(parent_identity),
        expected_target_identity: Some(target_identity),
        identity: target_identity,
        operation_nonce: nonce,
    })
}

fn bound_temporary_name(
    parent_identity: FileIdentity,
    expected_target_identity: Option<FileIdentity>,
    identity: FileIdentity,
    nonce: &[u8; 16],
) -> String {
    let target = expected_target_identity.unwrap_or(FileIdentity {
        volume_or_device: 0,
        file_or_inode: 0,
    });
    format!(
        ".devmanager-file-{:016x}-{:016x}-{:016x}-{:016x}-{:016x}-{:016x}-{}.tmp",
        parent_identity.volume_or_device,
        parent_identity.file_or_inode,
        target.volume_or_device,
        target.file_or_inode,
        identity.volume_or_device,
        identity.file_or_inode,
        encode_nonce(nonce),
    )
}

fn parse_temporary_binding(name: &str) -> Option<CleanupNameBinding> {
    let rest = strip_ascii_case_insensitive_prefix(name, ".devmanager-file-")?;
    let rest = strip_ascii_case_insensitive_suffix(rest, ".tmp")?;
    let parts = rest.split('-').collect::<Vec<_>>();
    if parts.len() != 7 {
        return None;
    }
    let parent_identity = parse_identity(parts[0], parts[1])?;
    let target = parse_identity(parts[2], parts[3])?;
    let identity = parse_identity(parts[4], parts[5])?;
    if identity_is_zero(parent_identity) || identity_is_zero(identity) {
        return None;
    }
    let operation_nonce = parse_nonce(parts[6])?;
    Some(CleanupNameBinding {
        parent_identity: Some(parent_identity),
        // A zero target is an explicit missing-target marker, not an absent
        // binding. It remains part of the durable name so restart recovery
        // can distinguish a new-file temporary from an attacker-chosen name.
        expected_target_identity: Some(target),
        identity,
        operation_nonce,
    })
}

fn parse_tombstone_identity(name: &str) -> Option<FileIdentity> {
    parse_tombstone_binding(name).map(|binding| binding.identity)
}

fn strip_ascii_case_insensitive_prefix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let head = name.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| name.get(prefix.len()..))?
}

fn strip_ascii_case_insensitive_suffix<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let start = name.len().checked_sub(suffix.len())?;
    let tail = name.get(start..)?;
    tail.eq_ignore_ascii_case(suffix)
        .then(|| name.get(..start))?
}

#[cfg(test)]
pub(crate) fn test_tombstone_name(
    parent_identity: FileIdentity,
    identity: FileIdentity,
    nonce: [u8; 16],
) -> String {
    format!(
        "{TOMBSTONE_PREFIX}{:016x}-{:016x}-{:016x}-{:016x}-{}.old",
        parent_identity.volume_or_device,
        parent_identity.file_or_inode,
        identity.volume_or_device,
        identity.file_or_inode,
        encode_nonce(&nonce),
    )
}

#[cfg(test)]
pub(crate) fn test_temporary_name(
    parent_identity: FileIdentity,
    expected_target_identity: Option<FileIdentity>,
    temporary: &File,
    nonce: [u8; 16],
) -> String {
    let identity = opened_file_info(temporary)
        .expect("test temporary identity")
        .0;
    bound_temporary_name(parent_identity, expected_target_identity, identity, &nonce)
}

#[cfg(test)]
mod secret_and_utf8_tests {
    use super::{SecretClassification, WorkspaceFileService};

    #[test]
    fn classify_secret_path_matches_existing_policy() {
        assert_eq!(
            WorkspaceFileService::classify_secret_path("README.md"),
            SecretClassification::Ordinary
        );
        assert_eq!(
            WorkspaceFileService::classify_secret_path(".env"),
            SecretClassification::SecretLike
        );
        assert_eq!(
            WorkspaceFileService::classify_secret_path("id_rsa"),
            SecretClassification::SecretLike
        );
        assert_eq!(
            WorkspaceFileService::classify_secret_path("tokens/prod.pem"),
            SecretClassification::SecretLike
        );
    }

    #[test]
    fn bounded_utf8_prefix_does_not_split_characters() {
        let bytes = "aé🎉".as_bytes();
        let prefix = WorkspaceFileService::bounded_utf8_prefix(bytes, 3).expect("prefix");
        assert!(prefix.len() <= 3);
        assert!(prefix.is_char_boundary(prefix.len()));
        assert!(!prefix.contains('🎉'));
        assert!(WorkspaceFileService::bounded_utf8_prefix(&[0x80, 0xFF], 8).is_none());
    }
}
