//! Visible-window native preview capture.
//!
//! Headless GPUI remains useful for structural tests, but it is not a visual
//! capture surface. The Windows path below owns the short-lived visible GPUI
//! window, captures its exact HWND, and writes only a first physical frame.

use image::ImageEncoder;
use std::cell::Cell;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
#[cfg(test)]
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ui::preview::{PreviewRequest, PreviewRoot};

/// End-to-end budget beginning when `capture_preview` admits the validated
/// request and covering GPUI setup, WGC startup, first frame, cleanup, join,
/// and PNG settlement.
pub const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(5);
pub const PREVIEW_WINDOW_WIDTH: i32 = 640;
pub const PREVIEW_WINDOW_HEIGHT: i32 = 360;

/// Maximum byte length of any capture diagnostic rendered for the UI.
pub const MAX_CLEANUP_DIAGNOSTIC_BYTES: usize = 4096;
/// Marker appended when a capture diagnostic reaches its byte or depth limit.
pub const CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER: &str = "... [truncated]";
/// Maximum recursive cleanup depth inspected while producing a diagnostic.
pub const MAX_CLEANUP_DIAGNOSTIC_DEPTH: usize = 16;

/// A monotonically fenced capture generation.  A caller that times out or
/// closes its preview cancels its lease; late WGC/PNG work may finish for
/// cleanup, but it can no longer publish a frame or result for that attempt.
#[derive(Clone)]
pub struct CaptureGeneration {
    coordinator: Arc<CaptureCoordinator>,
}

impl Default for CaptureGeneration {
    fn default() -> Self {
        Self::new()
    }
}

struct CaptureCoordinator {
    /// Generation admission, cancellation, final file mutation, result CAS,
    /// and outward publication all serialize on this lock.  A replacement
    /// generation therefore cannot enter the gap between validation and the
    /// irreversible rename.
    publication_lock: Mutex<()>,
    next: Mutex<u64>,
}

const PUBLICATION_IDLE: u8 = 0;
const PUBLICATION_IN_FLIGHT: u8 = 1;
const PUBLICATION_COMMITTED: u8 = 2;

impl CaptureGeneration {
    pub fn new() -> Self {
        Self {
            coordinator: Arc::new(CaptureCoordinator {
                publication_lock: Mutex::new(()),
                next: Mutex::new(0),
            }),
        }
    }

    pub fn begin(&self) -> CaptureLease {
        let _publication = self
            .coordinator
            .publication_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = self
            .coordinator
            .next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *next = next.saturating_add(1);
        let id = *next;
        CaptureLease {
            coordinator: Arc::clone(&self.coordinator),
            id,
            cancelled: Arc::new(AtomicBool::new(false)),
            publication_state: Arc::new(AtomicU8::new(PUBLICATION_IDLE)),
        }
    }
}

#[derive(Clone)]
pub struct CaptureLease {
    coordinator: Arc<CaptureCoordinator>,
    id: u64,
    cancelled: Arc<AtomicBool>,
    publication_state: Arc<AtomicU8>,
}

impl std::fmt::Debug for CaptureLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureLease")
            .field("active", &self.is_active())
            .finish()
    }
}

impl std::fmt::Display for CaptureLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CaptureLease(<opaque>)")
    }
}

impl CaptureLease {
    pub fn cancel(&self) {
        let _publication = self
            .coordinator
            .publication_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        let _publication = self
            .coordinator
            .publication_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.is_active_locked()
    }

    fn is_active_locked(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self
                .coordinator
                .next
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .eq(&self.id)
    }

    fn check(&self, deadline: CaptureDeadline) -> Result<(), PreviewCaptureError> {
        deadline.remaining()?;
        if self.is_active() {
            Ok(())
        } else {
            Err(PreviewCaptureError::CaptureCancelled)
        }
    }

    /// Execute the final publication mutation and its result commit while the
    /// generation/cancellation lock is held.  This is the only path allowed
    /// to cross the rename boundary.
    fn publish_with<T, F>(
        &self,
        deadline: CaptureDeadline,
        action: F,
    ) -> Result<T, PreviewCaptureError>
    where
        F: FnOnce() -> Result<T, PreviewCaptureError>,
    {
        self.check(deadline)?;
        let _publication = self
            .coordinator
            .publication_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Admission may have waited behind another generation's final
        // mutation.  Recheck the shared deadline after taking the lock so a
        // late caller cannot start a new irreversible mutation.
        deadline.remaining()?;
        if !self.is_active_locked() {
            return Err(PreviewCaptureError::CaptureCancelled);
        }
        self.publication_state
            .compare_exchange(
                PUBLICATION_IDLE,
                PUBLICATION_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| PreviewCaptureError::CaptureCancelled)?;
        match action() {
            Ok(value) if self.is_active_locked() => {
                self.publication_state
                    .store(PUBLICATION_COMMITTED, Ordering::Release);
                Ok(value)
            }
            Ok(_) => {
                self.publication_state
                    .store(PUBLICATION_IDLE, Ordering::Release);
                Err(PreviewCaptureError::CaptureCancelled)
            }
            Err(error) => {
                self.publication_state
                    .store(PUBLICATION_IDLE, Ordering::Release);
                Err(error)
            }
        }
    }
}

/// One absolute capture deadline. Every blocking capture phase consumes the
/// remaining time from this same instant; callers must not create phase-local
/// waits behind it.
#[derive(Debug, Clone, Copy)]
pub struct CaptureDeadline {
    deadline: Instant,
}

impl CaptureDeadline {
    pub fn from_now(budget: Duration) -> Self {
        Self {
            deadline: Instant::now()
                .checked_add(budget)
                .unwrap_or_else(Instant::now),
        }
    }

    pub fn remaining(self) -> Result<Duration, PreviewCaptureError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .ok_or(PreviewCaptureError::DeadlineExceeded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureCleanupOperation {
    Stop,
    Wait,
}

impl CaptureCleanupOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Wait => "wait",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureColorFormat {
    Bgra8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSetting {
    Excluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureContract {
    pub color_format: CaptureColorFormat,
    pub cursor: CaptureSetting,
    pub border: CaptureSetting,
    pub secondary_windows: CaptureSetting,
}

pub const fn capture_contract() -> CaptureContract {
    CaptureContract {
        color_format: CaptureColorFormat::Bgra8,
        cursor: CaptureSetting::Excluded,
        border: CaptureSetting::Excluded,
        secondary_windows: CaptureSetting::Excluded,
    }
}

static ACTIVE_CAPTURE_THREADS: AtomicUsize = AtomicUsize::new(0);
static TEMP_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static IN_CAPTURE_EXECUTOR_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// All potentially blocking preview work runs through this fixed executor.
/// A timed-out caller drops only its result receiver; the queued job remains
/// owned by one of these workers until it observes cancellation and settles.
pub const CAPTURE_EXECUTOR_WORKERS: usize = 4;
const CAPTURE_EXECUTOR_QUEUE: usize = 32;
const CAPTURE_EXECUTOR_SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);
type CaptureJobFn = Box<dyn FnOnce() + Send + 'static>;
type PublicationCallback = Box<dyn FnOnce() + Send + 'static>;

enum CaptureJob {
    Run(CaptureJobFn),
    Shutdown,
}

fn in_capture_executor_worker() -> bool {
    IN_CAPTURE_EXECUTOR_WORKER.with(Cell::get)
}

fn run_in_capture_executor_worker<F: FnOnce()>(job: F) {
    IN_CAPTURE_EXECUTOR_WORKER.with(|in_worker| {
        let was_in_worker = in_worker.replace(true);
        job();
        in_worker.set(was_in_worker);
    });
}

struct CaptureExecutor {
    sender: SyncSender<CaptureJob>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    shutdown_requested: Arc<AtomicBool>,
}

struct CaptureTask<T> {
    receiver: Receiver<Result<T, PreviewCaptureError>>,
}

static CAPTURE_EXECUTOR: OnceLock<CaptureExecutor> = OnceLock::new();

/// File-system authority captured during request validation.  The original
/// root and parent handles stay alive through capture; publication reopens
/// both with no-follow semantics and compares exact identities before using
/// the reopened parent handle for the relative rename.
pub struct CaptureOutputAuthority {
    root_path: PathBuf,
    parent_path: PathBuf,
    /// Lexical path from the retained root to the retained publication
    /// parent.  The path is only an ancestry assertion; all later mutation
    /// uses the retained/reopened directory handle below.
    parent_relative_to_root: PathBuf,
    /// Every directory from the trusted root through the publication parent.
    /// Keeping this chain open prevents a swapped intermediate directory from
    /// becoming the authority between validation and publication.
    ancestor_paths: Vec<PathBuf>,
    ancestor_identities: Vec<CaptureFileIdentity>,
    output_name: OsString,
    root_identity: CaptureFileIdentity,
    parent_identity: CaptureFileIdentity,
    #[cfg(windows)]
    root_handle: Arc<std::os::windows::io::OwnedHandle>,
    #[cfg(windows)]
    parent_handle: Arc<std::os::windows::io::OwnedHandle>,
    #[cfg(unix)]
    root_handle: Arc<std::os::fd::OwnedFd>,
    #[cfg(unix)]
    parent_handle: Arc<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    ancestor_handles: Vec<Arc<std::os::windows::io::OwnedHandle>>,
    #[cfg(unix)]
    ancestor_handles: Vec<Arc<std::os::fd::OwnedFd>>,
}

impl PartialEq for CaptureOutputAuthority {
    fn eq(&self, other: &Self) -> bool {
        self.output_name == other.output_name
            && self.root_identity == other.root_identity
            && self.parent_identity == other.parent_identity
    }
}

impl Eq for CaptureOutputAuthority {}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CaptureFileIdentity {
    #[cfg(windows)]
    volume_serial: u32,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(all(not(windows), unix))]
    dev: u64,
    #[cfg(all(not(windows), unix))]
    inode: u64,
    #[cfg(all(not(windows), not(unix)))]
    modified_nanos: u128,
    #[cfg(all(not(windows), not(unix)))]
    length: u64,
}

/// The exact inode/file object published for an attempt.  The handle remains
/// owned by the attempt until the generation commit succeeds, so cleanup can
/// never infer ownership from a final name that another process may have
/// replaced.
struct PublishedOutput {
    final_output_handle: std::fs::File,
    final_output_identity: CaptureFileIdentity,
}

impl PublishedOutput {
    fn from_handle(final_output_handle: std::fs::File) -> Result<Self, PreviewCaptureError> {
        let final_output_identity = file_identity_from_handle(&final_output_handle)?;
        Ok(Self {
            final_output_handle,
            final_output_identity,
        })
    }

    fn verify_published_output_identity(
        &self,
        authority: &CaptureOutputAuthority,
    ) -> Result<(), PreviewCaptureError> {
        authority.verify_published_output_identity(self)
    }

    fn delete_published_output_by_handle(
        &self,
        authority: &CaptureOutputAuthority,
    ) -> Result<(), PreviewCaptureError> {
        #[cfg(windows)]
        {
            let _ = authority;
            delete_published_output_by_handle(&self.final_output_handle).map_err(|error| {
                PreviewCaptureError::OutputFailed(format!(
                    "handle-relative published output cleanup failed ({error})"
                ))
            })
        }
        #[cfg(unix)]
        {
            let _ = authority;
            // Unix has no unlink-by-file-descriptor primitive.  A name check
            // followed by unlinkat would be a TOCTOU deletion primitive: an
            // attacker can swap the final name after the check.  Leave the
            // exact residue visible rather than risking a replacement.
            Err(PreviewCaptureError::OutputFailed(
                "output residue is unresolved".into(),
            ))
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = authority;
            Err(PreviewCaptureError::OutputFailed(
                "output residue is unresolved".into(),
            ))
        }
    }
}

impl std::fmt::Debug for CaptureOutputAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CaptureOutputAuthority(<opaque>)")
    }
}

impl std::fmt::Display for CaptureOutputAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CaptureOutputAuthority(<opaque>)")
    }
}

fn ancestor_chain_change_error(index: usize, length: usize) -> PreviewCaptureError {
    let message = if index == 0 {
        "trusted preview output root changed during capture"
    } else if index + 1 == length {
        "trusted preview output parent changed during capture"
    } else {
        "trusted preview output ancestor changed during capture"
    };
    PreviewCaptureError::OutputFailed(message.into())
}

impl CaptureOutputAuthority {
    pub fn new(output: &Path, trusted_root: &Path) -> Result<Self, PreviewCaptureError> {
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| {
                PreviewCaptureError::OutputFailed("PNG output has no parent directory".into())
            })?;
        let output_name = output
            .file_name()
            .ok_or_else(|| PreviewCaptureError::OutputFailed("PNG output has no file name".into()))?
            .to_os_string();
        let root_path = trusted_root.to_path_buf();
        let parent_path = parent.to_path_buf();
        let parent_relative_to_root = verify_parent_descendant(&root_path, &parent_path)?;
        let ancestor_paths = directory_path_chain(&root_path, &parent_path)?;
        #[cfg(windows)]
        {
            let mut opened = ancestor_paths
                .iter()
                .map(|path| open_directory_authority(path))
                .collect::<Result<Vec<_>, _>>()?;
            opened.pop();
            opened.push(open_directory_authority_for_publication(&parent_path)?);
            let ancestor_identities: Vec<CaptureFileIdentity> =
                opened.iter().map(|(_, identity)| *identity).collect();
            let ancestor_handles = opened
                .into_iter()
                .map(|(handle, _)| Arc::new(handle))
                .collect::<Vec<_>>();
            let root_handle = Arc::clone(&ancestor_handles[0]);
            let parent_handle = Arc::clone(ancestor_handles.last().expect("ancestor chain"));
            let root_identity = *ancestor_identities.first().expect("ancestor identity");
            let parent_identity = *ancestor_identities.last().expect("ancestor identity");
            return Ok(Self {
                root_path,
                parent_path,
                parent_relative_to_root,
                ancestor_paths,
                ancestor_identities,
                output_name,
                root_identity,
                parent_identity,
                root_handle,
                parent_handle,
                ancestor_handles,
            });
        }
        #[cfg(not(windows))]
        {
            #[cfg(unix)]
            {
                let opened = ancestor_paths
                    .iter()
                    .map(|path| open_directory_authority(path))
                    .collect::<Result<Vec<_>, _>>()?;
                let ancestor_identities: Vec<CaptureFileIdentity> =
                    opened.iter().map(|(_, identity)| *identity).collect();
                let ancestor_handles = opened
                    .into_iter()
                    .map(|(handle, _)| Arc::new(handle))
                    .collect::<Vec<_>>();
                let root_handle = Arc::clone(&ancestor_handles[0]);
                let parent_handle = Arc::clone(ancestor_handles.last().expect("ancestor chain"));
                let root_identity = *ancestor_identities.first().expect("ancestor identity");
                let parent_identity = *ancestor_identities.last().expect("ancestor identity");
                return Ok(Self {
                    root_path,
                    parent_path,
                    parent_relative_to_root,
                    ancestor_paths,
                    ancestor_identities,
                    output_name,
                    root_identity,
                    parent_identity,
                    root_handle,
                    parent_handle,
                    ancestor_handles,
                });
            }
            #[cfg(not(unix))]
            {
                let ancestor_identities = ancestor_paths
                    .iter()
                    .map(|path| capture_file_identity(path))
                    .collect::<Result<Vec<_>, _>>()?;
                let root_identity = *ancestor_identities.first().expect("ancestor identity");
                let parent_identity = *ancestor_identities.last().expect("ancestor identity");
                Ok(Self {
                    root_path,
                    parent_path,
                    parent_relative_to_root,
                    ancestor_paths,
                    ancestor_identities,
                    output_name,
                    root_identity,
                    parent_identity,
                })
            }
        }
    }

    pub(crate) fn output_path(&self) -> PathBuf {
        self.parent_path.join(&self.output_name)
    }

    fn verify_parent_descendant(&self) -> Result<(), PreviewCaptureError> {
        let relative = verify_parent_descendant(&self.root_path, &self.parent_path)?;
        if relative != self.parent_relative_to_root {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output parent is no longer beneath its root".into(),
            ));
        }
        Ok(())
    }

    fn verify_ancestor_chain(&self) -> Result<(), PreviewCaptureError> {
        self.verify_parent_descendant()?;
        if self.ancestor_paths.len() != self.ancestor_identities.len() {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output ancestor chain is incomplete".into(),
            ));
        }
        #[cfg(windows)]
        {
            if self.ancestor_handles.len() != self.ancestor_paths.len() {
                return Err(PreviewCaptureError::OutputFailed(
                    "trusted preview output ancestor handles are incomplete".into(),
                ));
            }
            for (index, ((path, expected), handle)) in self
                .ancestor_paths
                .iter()
                .zip(&self.ancestor_identities)
                .zip(&self.ancestor_handles)
                .enumerate()
            {
                let (_, current) = open_directory_authority(path)
                    .map_err(|_| ancestor_chain_change_error(index, self.ancestor_paths.len()))?;
                let retained = directory_identity_from_handle(handle)
                    .map_err(|_| ancestor_chain_change_error(index, self.ancestor_paths.len()))?;
                if current != *expected || retained != *expected {
                    return Err(ancestor_chain_change_error(
                        index,
                        self.ancestor_paths.len(),
                    ));
                }
            }
        }
        #[cfg(unix)]
        {
            if self.ancestor_handles.len() != self.ancestor_paths.len() {
                return Err(PreviewCaptureError::OutputFailed(
                    "trusted preview output ancestor handles are incomplete".into(),
                ));
            }
            for (index, ((path, expected), handle)) in self
                .ancestor_paths
                .iter()
                .zip(&self.ancestor_identities)
                .zip(&self.ancestor_handles)
                .enumerate()
            {
                let (_, current) = open_directory_authority(path)
                    .map_err(|_| ancestor_chain_change_error(index, self.ancestor_paths.len()))?;
                let retained = directory_identity_from_handle(handle)
                    .map_err(|_| ancestor_chain_change_error(index, self.ancestor_paths.len()))?;
                if current != *expected || retained != *expected {
                    return Err(ancestor_chain_change_error(
                        index,
                        self.ancestor_paths.len(),
                    ));
                }
            }
        }
        #[cfg(not(any(windows, unix)))]
        {
            for (index, (path, expected)) in self
                .ancestor_paths
                .iter()
                .zip(&self.ancestor_identities)
                .enumerate()
            {
                if capture_file_identity(path).ok() != Some(*expected) {
                    return Err(ancestor_chain_change_error(
                        index,
                        self.ancestor_paths.len(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn remove_output_relative(&self) -> Result<(), PreviewCaptureError> {
        self.remove_name_relative(&self.output_name)
    }

    fn output_identity_relative(&self) -> Result<CaptureFileIdentity, PreviewCaptureError> {
        self.verify_ancestor_chain()?;
        #[cfg(windows)]
        {
            let parent = self.reopen_parent_for_publication()?;
            let output =
                open_relative_windows_existing(&parent, &self.output_name).map_err(|_| {
                    PreviewCaptureError::OutputFailed("output residue is unresolved".into())
                })?;
            return file_identity_from_handle(&output);
        }
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;

            let name = CString::new(self.output_name.as_bytes()).map_err(|_| {
                PreviewCaptureError::OutputFailed("output residue is unresolved".into())
            })?;
            let parent = self.reopen_parent_for_publication()?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(PreviewCaptureError::OutputFailed(
                    "output residue is unresolved".into(),
                ));
            }
            let output = std::fs::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            return file_identity_from_handle(&output);
        }
        #[cfg(not(any(windows, unix)))]
        {
            Err(PreviewCaptureError::OutputFailed(
                "output residue is unresolved".into(),
            ))
        }
    }

    fn verify_published_output_identity(
        &self,
        published: &PublishedOutput,
    ) -> Result<(), PreviewCaptureError> {
        #[cfg(windows)]
        {
            // Prefer a handle-relative identity lookup.  Some hardened
            // directories deny a second child open even to the publishing
            // process; the retained handle's final kernel path is the
            // fail-closed fallback in that case and still detects a moved
            // original before a replacement can be reported as committed.
            if let Ok(current) = self.output_identity_relative() {
                if current != published.final_output_identity {
                    return Err(PreviewCaptureError::OutputFailed(
                        "output residue is unresolved".into(),
                    ));
                }
            }
            return verify_windows_published_output_path(
                &published.final_output_handle,
                &self.output_path(),
            );
        }
        #[cfg(not(windows))]
        {
            let current = self.output_identity_relative().map_err(|_| {
                PreviewCaptureError::OutputFailed("output residue is unresolved".into())
            })?;
            if current == published.final_output_identity {
                Ok(())
            } else {
                Err(PreviewCaptureError::OutputFailed(
                    "output residue is unresolved".into(),
                ))
            }
        }
    }

    fn remove_temp_relative(&self, name: &OsString) -> Result<(), PreviewCaptureError> {
        self.remove_name_relative(name)
    }

    fn remove_name_relative(&self, name: &OsString) -> Result<(), PreviewCaptureError> {
        self.verify_parent_descendant()?;
        #[cfg(windows)]
        {
            let parent = self.reopen_parent_for_publication()?;
            return delete_relative_windows(&parent, name).map_err(|error| {
                PreviewCaptureError::OutputFailed(format!(
                    "handle-relative preview cleanup failed ({error})"
                ))
            });
        }
        #[cfg(unix)]
        {
            let parent = self.reopen_parent_for_publication()?;
            return unlink_relative_unix(&parent, name).map_err(|error| {
                PreviewCaptureError::OutputFailed(format!(
                    "handle-relative preview cleanup failed ({error})"
                ))
            });
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = name;
            Err(PreviewCaptureError::OutputFailed(
                "handle-relative preview cleanup is unsupported on this platform".into(),
            ))
        }
    }

    fn verify_reopened(&self) -> Result<(), PreviewCaptureError> {
        self.verify_ancestor_chain()?;
        #[cfg(windows)]
        {
            let _ = self.reopen_parent_for_publication()?;
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            #[cfg(unix)]
            {
                let _ = self.reopen_parent_for_publication()?;
                return Ok(());
            }
            #[cfg(not(unix))]
            {
                if capture_file_identity(&self.root_path).ok() != Some(self.root_identity)
                    || capture_file_identity(&self.parent_path).ok() != Some(self.parent_identity)
                {
                    return Err(PreviewCaptureError::OutputFailed(
                        "trusted preview output authority changed during capture".into(),
                    ));
                }
                Ok(())
            }
        }
    }

    #[cfg(windows)]
    fn reopen_parent_for_publication(
        &self,
    ) -> Result<std::os::windows::io::OwnedHandle, PreviewCaptureError> {
        self.verify_ancestor_chain()?;
        self.parent_handle.try_clone().map_err(|_| {
            PreviewCaptureError::OutputFailed(
                "trusted preview output parent handle changed during capture".into(),
            )
        })
    }

    #[cfg(unix)]
    fn reopen_parent_for_publication(&self) -> Result<std::os::fd::OwnedFd, PreviewCaptureError> {
        self.verify_ancestor_chain()?;
        self.parent_handle.try_clone().map_err(|_| {
            PreviewCaptureError::OutputFailed(
                "trusted preview output parent handle changed during capture".into(),
            )
        })
    }
}

fn verify_parent_descendant(root: &Path, parent: &Path) -> Result<PathBuf, PreviewCaptureError> {
    #[cfg(windows)]
    {
        let normalize = |path: &Path| {
            let value = path.to_string_lossy();
            value
                .strip_prefix(r"\\?\")
                .unwrap_or(&value)
                .trim_end_matches(['\\', '/'])
                .to_ascii_lowercase()
                .replace('/', "\\")
        };
        let root = normalize(root);
        let parent = normalize(parent);
        if parent == root {
            return Ok(PathBuf::new());
        }
        let prefix = format!(r"{root}\");
        if !parent.starts_with(&prefix) {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output parent is outside its root".into(),
            ));
        }
        return Ok(PathBuf::from(&parent[prefix.len()..]));
    }
    #[cfg(not(windows))]
    {
        let root = root.components().collect::<Vec<_>>();
        let parent = parent.components().collect::<Vec<_>>();
        if root.len() > parent.len() {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output parent is outside its root".into(),
            ));
        }
        let same_prefix = root.iter().zip(parent.iter()).all(|(root, parent)| {
            #[cfg(windows)]
            {
                root.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&parent.as_os_str().to_string_lossy())
            }
            #[cfg(not(windows))]
            {
                root == parent
            }
        });
        if !same_prefix {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output parent is outside its root".into(),
            ));
        }
        let relative =
            parent
                .iter()
                .skip(root.len())
                .fold(PathBuf::new(), |mut relative, component| {
                    relative.push(component.as_os_str());
                    relative
                });
        Ok(relative)
    }
}

fn directory_path_chain(root: &Path, parent: &Path) -> Result<Vec<PathBuf>, PreviewCaptureError> {
    verify_parent_descendant(root, parent)?;
    let normalize = |path: &Path| {
        #[cfg(windows)]
        {
            let value = path.to_string_lossy();
            value
                .strip_prefix(r"\\?\")
                .unwrap_or(&value)
                .trim_end_matches(['\\', '/'])
                .to_ascii_lowercase()
                .replace('/', "\\")
        }
        #[cfg(not(windows))]
        {
            path.to_string_lossy().trim_end_matches('/').to_string()
        }
    };
    let root_key = normalize(root);
    let mut cursor = parent.to_path_buf();
    let mut reverse = Vec::new();
    loop {
        reverse.push(cursor.clone());
        if normalize(&cursor) == root_key {
            reverse.reverse();
            return Ok(reverse);
        }
        let next = cursor.parent().map(Path::to_path_buf).ok_or_else(|| {
            PreviewCaptureError::OutputFailed(
                "trusted preview output ancestor chain could not reach its root".into(),
            )
        })?;
        if next == cursor {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output ancestor chain could not reach its root".into(),
            ));
        }
        cursor = next;
    }
}

#[cfg(unix)]
fn unlink_relative_unix(parent: &std::os::fd::OwnedFd, name: &OsString) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let name = CString::new(name.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preview cleanup name contains NUL",
        )
    })?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(windows)]
#[repr(C)]
struct NativeIoStatusBlock {
    status: i32,
    information: usize,
}

#[cfg(windows)]
fn delete_relative_windows(
    parent: &std::os::windows::io::OwnedHandle,
    name: &OsString,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;

    #[repr(C)]
    struct NativeUnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct NativeObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut NativeUnicodeString,
        attributes: u32,
        security_descriptor: *mut std::ffi::c_void,
        security_quality_of_service: *mut std::ffi::c_void,
    }

    #[repr(C)]
    struct NativeFileDispositionInformation {
        delete_file: u8,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *mut NativeObjectAttributes,
            io_status_block: *mut NativeIoStatusBlock,
            allocation_size: *mut i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut std::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut NativeIoStatusBlock,
            file_information: *const std::ffi::c_void,
            length: u32,
            file_information_class: i32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_OPEN: u32 = 0x0000_0001;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
    const FILE_DISPOSITION_INFORMATION: i32 = 13;

    let mut wide: Vec<u16> = name.encode_wide().collect();
    let byte_length = wide
        .len()
        .checked_mul(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preview cleanup name too long",
            )
        })?;
    let mut unicode = NativeUnicodeString {
        length: byte_length,
        maximum_length: byte_length,
        buffer: wide.as_mut_ptr(),
    };
    let mut attributes = NativeObjectAttributes {
        length: u32::try_from(std::mem::size_of::<NativeObjectAttributes>()).unwrap_or(u32::MAX),
        root_directory: HANDLE(parent.as_raw_handle()),
        object_name: &mut unicode,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut io_status = NativeIoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut raw = HANDLE::default();
    let mut allocation_size = 0_i64;
    let status = unsafe {
        NtCreateFile(
            &mut raw,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &mut attributes,
            &mut io_status,
            &mut allocation_size,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        let error = unsafe { RtlNtStatusToDosError(status) };
        let error = std::io::Error::from_raw_os_error(i32::try_from(error).unwrap_or(i32::MAX));
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.0 as *mut std::ffi::c_void) };
    let disposition = NativeFileDispositionInformation { delete_file: 1 };
    let status = unsafe {
        NtSetInformationFile(
            HANDLE(handle.as_raw_handle()),
            &mut io_status,
            (&disposition as *const NativeFileDispositionInformation).cast(),
            u32::try_from(std::mem::size_of::<NativeFileDispositionInformation>())
                .unwrap_or(u32::MAX),
            FILE_DISPOSITION_INFORMATION,
        )
    };
    if status < 0 {
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(std::io::Error::from_raw_os_error(
            i32::try_from(error).unwrap_or(i32::MAX),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn delete_published_output_by_handle(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;

    #[repr(C)]
    struct NativeFileDispositionInformation {
        delete_file: u8,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut NativeIoStatusBlock,
            file_information: *const std::ffi::c_void,
            length: u32,
            file_information_class: i32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    const FILE_DISPOSITION_INFORMATION: i32 = 13;
    let disposition = NativeFileDispositionInformation { delete_file: 1 };
    let mut io_status = NativeIoStatusBlock {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtSetInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut io_status,
            (&disposition as *const NativeFileDispositionInformation).cast(),
            u32::try_from(std::mem::size_of::<NativeFileDispositionInformation>())
                .unwrap_or(u32::MAX),
            FILE_DISPOSITION_INFORMATION,
        )
    };
    if status < 0 {
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(std::io::Error::from_raw_os_error(
            i32::try_from(error).unwrap_or(i32::MAX),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_relative_windows(
    parent: &std::os::windows::io::OwnedHandle,
    name: &OsString,
) -> std::io::Result<std::fs::File> {
    // Keep the production source handle delete-exclusive.  The retained
    // handle therefore fences a final-name swap for the whole commit window;
    // the dedicated source-swap unit test uses its own test-only opener.
    const FILE_CREATE: u32 = 0x0000_0002;
    const FILE_READ_DATA: u32 = 0x0000_0001;
    const FILE_WRITE_DATA: u32 = 0x0000_0002;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const DELETE: u32 = 0x0001_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    open_relative_windows_with_disposition(
        parent,
        name,
        FILE_CREATE,
        FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE,
        0x0000_0001 | 0x0000_0002,
    )
}

#[cfg(windows)]
fn open_relative_windows_existing(
    parent: &std::os::windows::io::OwnedHandle,
    name: &OsString,
) -> std::io::Result<std::fs::File> {
    const FILE_OPEN: u32 = 0x0000_0001;
    const FILE_READ_DATA: u32 = 0x0000_0001;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    open_relative_windows_with_disposition(
        parent,
        name,
        FILE_OPEN,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        0x0000_0001 | 0x0000_0002,
    )
}

#[cfg(windows)]
fn open_relative_windows_with_disposition(
    parent: &std::os::windows::io::OwnedHandle,
    name: &OsString,
    create_disposition: u32,
    desired_access: u32,
    share_access: u32,
) -> std::io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;

    #[repr(C)]
    struct NativeUnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct NativeObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut NativeUnicodeString,
        attributes: u32,
        security_descriptor: *mut std::ffi::c_void,
        security_quality_of_service: *mut std::ffi::c_void,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *mut NativeObjectAttributes,
            io_status_block: *mut NativeIoStatusBlock,
            allocation_size: *mut i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut std::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;

    let mut wide: Vec<u16> = name.encode_wide().collect();
    let byte_length = wide
        .len()
        .checked_mul(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preview temp name too long",
            )
        })?;
    let mut unicode = NativeUnicodeString {
        length: byte_length,
        maximum_length: byte_length,
        buffer: wide.as_mut_ptr(),
    };
    let mut attributes = NativeObjectAttributes {
        length: u32::try_from(std::mem::size_of::<NativeObjectAttributes>()).unwrap_or(u32::MAX),
        root_directory: HANDLE(parent.as_raw_handle()),
        object_name: &mut unicode,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut io_status = NativeIoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut raw = HANDLE::default();
    let mut allocation_size = 0_i64;
    let status = unsafe {
        NtCreateFile(
            &mut raw,
            desired_access,
            &mut attributes,
            &mut io_status,
            &mut allocation_size,
            0,
            share_access,
            create_disposition,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(std::io::Error::from_raw_os_error(
            i32::try_from(error).unwrap_or(i32::MAX),
        ));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.0 as *mut std::ffi::c_void) };
    Ok(std::fs::File::from(handle))
}

#[cfg(windows)]
fn directory_identity_from_handle(
    handle: &std::os::windows::io::OwnedHandle,
) -> Result<CaptureFileIdentity, PreviewCaptureError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle()), &mut information) }
        .map_err(|error| {
            PreviewCaptureError::OutputFailed(format!(
                "output authority identity failed ({})",
                error.code().0
            ))
        })?;
    Ok(CaptureFileIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

#[cfg(windows)]
fn file_identity_from_handle(
    file: &std::fs::File,
) -> Result<CaptureFileIdentity, PreviewCaptureError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(|_| PreviewCaptureError::OutputFailed("output residue is unresolved".into()))?;
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
        != 0
    {
        return Err(PreviewCaptureError::OutputFailed(
            "output residue is unresolved".into(),
        ));
    }
    Ok(CaptureFileIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

#[cfg(windows)]
fn verify_windows_published_output_path(
    file: &std::fs::File,
    expected_path: &Path,
) -> Result<(), PreviewCaptureError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file_handle: HANDLE,
            file_path: *mut u16,
            file_path_length: u32,
            flags: u32,
        ) -> u32;
        fn GetFullPathNameW(
            file_name: *const u16,
            buffer_length: u32,
            buffer: *mut u16,
            file_part: *mut *mut u16,
        ) -> u32;
    }

    fn read_final_path(file: &std::fs::File) -> std::io::Result<Vec<u16>> {
        let mut buffer = vec![0_u16; 512];
        loop {
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    HANDLE(file.as_raw_handle()),
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    0,
                )
            };
            if length == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(buffer);
            }
            if length >= 32_768 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "preview output handle path is too long",
                ));
            }
            buffer.resize(length.saturating_add(1), 0);
        }
    }

    fn read_full_path(path: &Path) -> std::io::Result<Vec<u16>> {
        let mut input: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buffer = vec![0_u16; 512];
        loop {
            let mut file_part = std::ptr::null_mut();
            let length = unsafe {
                GetFullPathNameW(
                    input.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    buffer.as_mut_ptr(),
                    &mut file_part,
                )
            };
            if length == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(buffer);
            }
            if length >= 32_768 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "preview output path is too long",
                ));
            }
            buffer.resize(length.saturating_add(1), 0);
        }
    }

    fn normalize(mut path: Vec<u16>) -> Vec<u16> {
        if path.len() >= 8
            && path[0] == '\\' as u16
            && path[1] == '\\' as u16
            && path[2] == '?' as u16
            && path[3] == '\\' as u16
            && path[4] == 'U' as u16
            && path[5] == 'N' as u16
            && path[6] == 'C' as u16
            && path[7] == '\\' as u16
        {
            path.drain(..8);
            path.splice(0..0, ['\\' as u16, '\\' as u16]);
        } else if path.len() >= 4
            && path[0] == '\\' as u16
            && path[1] == '\\' as u16
            && path[2] == '?' as u16
            && path[3] == '\\' as u16
        {
            path.drain(..4);
        }
        path.iter_mut().for_each(|unit| {
            if (b'A' as u16..=b'Z' as u16).contains(unit) {
                *unit = unit.saturating_add(b'a' as u16 - b'A' as u16);
            }
        });
        path
    }

    let actual = read_final_path(file)
        .map_err(|_| PreviewCaptureError::OutputFailed("output residue is unresolved".into()))?;
    let expected = read_full_path(expected_path)
        .map_err(|_| PreviewCaptureError::OutputFailed("output residue is unresolved".into()))?;
    if normalize(actual) == normalize(expected) {
        Ok(())
    } else {
        Err(PreviewCaptureError::OutputFailed(
            "output residue is unresolved".into(),
        ))
    }
}

#[cfg(not(windows))]
fn capture_file_identity(path: &Path) -> std::io::Result<CaptureFileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "preview output authority is not a regular directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(CaptureFileIdentity {
            dev: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[cfg(not(unix))]
    {
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Ok(CaptureFileIdentity {
            modified_nanos,
            length: metadata.len(),
        })
    }
}

#[cfg(unix)]
fn file_identity_from_handle(
    file: &std::fs::File,
) -> Result<CaptureFileIdentity, PreviewCaptureError> {
    use std::os::fd::AsRawFd;

    let mut information = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(file.as_raw_fd(), information.as_mut_ptr()) };
    if result != 0 {
        return Err(PreviewCaptureError::OutputFailed(
            "output residue is unresolved".into(),
        ));
    }
    let information = unsafe { information.assume_init() };
    if information.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(PreviewCaptureError::OutputFailed(
            "output residue is unresolved".into(),
        ));
    }
    Ok(CaptureFileIdentity {
        dev: information.st_dev as u64,
        inode: information.st_ino as u64,
    })
}

#[cfg(not(any(windows, unix)))]
fn file_identity_from_handle(
    file: &std::fs::File,
) -> Result<CaptureFileIdentity, PreviewCaptureError> {
    let metadata = file
        .metadata()
        .map_err(|_| PreviewCaptureError::OutputFailed("output residue is unresolved".into()))?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    Ok(CaptureFileIdentity {
        modified_nanos,
        length: metadata.len(),
    })
}

#[cfg(windows)]
fn open_directory_authority(
    path: &Path,
) -> Result<(std::os::windows::io::OwnedHandle, CaptureFileIdentity), PreviewCaptureError> {
    open_directory_authority_with_access(
        path,
        (windows::Win32::Storage::FileSystem::FILE_TRAVERSE
            | windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES)
            .0,
    )
}

#[cfg(windows)]
fn open_directory_authority_for_publication(
    path: &Path,
) -> Result<(std::os::windows::io::OwnedHandle, CaptureFileIdentity), PreviewCaptureError> {
    open_directory_authority_with_access(
        path,
        (windows::Win32::Storage::FileSystem::FILE_TRAVERSE
            | windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES)
            .0
            | 0x0000_0002
            | 0x0000_0040,
    )
}

#[cfg(windows)]
fn open_directory_authority_with_access(
    path: &Path,
    desired_access: u32,
) -> Result<(std::os::windows::io::OwnedHandle, CaptureFileIdentity), PreviewCaptureError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let raw = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|error| {
        PreviewCaptureError::OutputFailed(format!(
            "output authority open failed ({})",
            error.code().0
        ))
    })?;
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.0 as *mut std::ffi::c_void) };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle()), &mut information) }
        .map_err(|error| {
            PreviewCaptureError::OutputFailed(format!(
                "output authority identity failed ({})",
                error.code().0
            ))
        })?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err(PreviewCaptureError::OutputFailed(
            "preview output authority is not a regular directory".into(),
        ));
    }
    Ok((
        handle,
        CaptureFileIdentity {
            volume_serial: information.dwVolumeSerialNumber,
            file_index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        },
    ))
}

#[cfg(unix)]
fn directory_identity_from_handle(
    handle: &std::os::fd::OwnedFd,
) -> Result<CaptureFileIdentity, PreviewCaptureError> {
    use std::os::fd::AsRawFd;
    let mut information = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(handle.as_raw_fd(), information.as_mut_ptr()) };
    if result != 0 {
        return Err(PreviewCaptureError::OutputFailed(
            "output authority identity failed".into(),
        ));
    }
    let information = unsafe { information.assume_init() };
    if information.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(PreviewCaptureError::OutputFailed(
            "preview output authority is not a regular directory".into(),
        ));
    }
    Ok(CaptureFileIdentity {
        dev: information.st_dev as u64,
        inode: information.st_ino as u64,
    })
}

#[cfg(unix)]
fn open_directory_authority(
    path: &Path,
) -> Result<(std::os::fd::OwnedFd, CaptureFileIdentity), PreviewCaptureError> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            PreviewCaptureError::OutputFailed(format!("output authority open failed ({})", error))
        })?;
    let raw = file.into_raw_fd();
    let handle = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    let identity = directory_identity_from_handle(&handle)?;
    Ok((handle, identity))
}

pub fn active_capture_thread_count() -> usize {
    ACTIVE_CAPTURE_THREADS.load(Ordering::SeqCst)
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn cancellation_cannot_enter_the_final_mutation_gap() {
        let generations = CaptureGeneration::new();
        let lease = generations.begin();
        let action_lease = lease.clone();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let publication = thread::spawn(move || {
            action_lease.publish_with(CaptureDeadline::from_now(Duration::from_secs(1)), || {
                entered_tx.send(()).expect("publication action entered");
                release_rx.recv().expect("publication release");
                Ok::<_, PreviewCaptureError>(())
            })
        });
        entered_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("publication action should enter");

        let cancellation_lease = lease.clone();
        let cancellation = thread::spawn(move || cancellation_lease.cancel());
        assert!(
            !cancellation.is_finished(),
            "cancellation must wait while the final mutation is in flight"
        );
        release_tx.send(()).expect("release publication action");
        assert_eq!(publication.join().expect("publication thread"), Ok(()));
        cancellation.join().expect("cancellation thread");
        assert!(
            !lease.is_active(),
            "cancellation applies after the commit boundary"
        );
    }

    #[test]
    fn replacement_generation_waits_for_and_fences_final_mutation() {
        let generations = CaptureGeneration::new();
        let stale = generations.begin();
        let action_lease = stale.clone();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let publication = thread::spawn(move || {
            action_lease.publish_with(CaptureDeadline::from_now(Duration::from_secs(1)), || {
                entered_tx.send(()).expect("publication action entered");
                release_rx.recv().expect("publication release");
                Ok::<_, PreviewCaptureError>(())
            })
        });
        entered_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("publication action should enter");

        let replacement_generation = generations.clone();
        let (replacement_tx, replacement_rx) = mpsc::sync_channel(1);
        let replacement = thread::spawn(move || {
            let lease = replacement_generation.begin();
            replacement_tx
                .send(lease.clone())
                .expect("replacement lease");
            lease
        });
        assert!(
            replacement_rx
                .recv_timeout(Duration::from_millis(25))
                .is_err(),
            "replacement admission must not enter the final mutation gap"
        );
        release_tx.send(()).expect("release publication action");
        assert_eq!(publication.join().expect("publication thread"), Ok(()));
        let replacement_lease = replacement.join().expect("replacement thread");
        assert!(replacement_lease.is_active());
        assert!(
            !stale.is_active(),
            "the replaced generation cannot publish afterward"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_publication_fences_replacement_until_rename_and_result_commit() {
        let root = tempfile::tempdir().expect("publication test temp root");
        let output = root.path().join("published.png");
        let authority = Arc::new(
            CaptureOutputAuthority::new(&output, root.path())
                .expect("publication test authority should open"),
        );
        let generations = CaptureGeneration::new();
        let stale = generations.begin();
        let worker_lease = stale.clone();
        let authority_for_worker = Arc::clone(&authority);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let publication = thread::spawn(move || {
            encode_bgra_png_atomic_with_authority_and_publication(
                authority_for_worker,
                1,
                1,
                &[0x1e, 0x14, 0x0a, 0xff],
                CaptureDeadline::from_now(Duration::from_secs(2)),
                &worker_lease,
                move || {
                    entered_tx.send(()).expect("file publication should enter");
                    release_rx.recv().expect("file publication release");
                },
            )
        });
        entered_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("file publication callback should run after rename");
        assert!(output.exists(), "rename must happen before outward commit");

        let replacement_generation = generations.clone();
        let (replacement_tx, replacement_rx) = mpsc::sync_channel(1);
        let replacement = thread::spawn(move || {
            let lease = replacement_generation.begin();
            replacement_tx
                .send(lease.clone())
                .expect("replacement lease");
            lease
        });
        assert!(
            replacement_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "replacement must not enter the rename/result publication gap"
        );
        release_tx.send(()).expect("release file publication");
        assert!(matches!(
            publication.join().expect("file publication thread"),
            Ok(()) | Err(PreviewCaptureError::CaptureCancelled)
        ));
        let replacement_lease = replacement.join().expect("replacement thread");
        assert!(replacement_lease.is_active());
        assert!(!stale.is_active());
    }

    #[cfg(windows)]
    #[test]
    fn windows_temp_path_swap_cannot_replace_the_open_source_handle() {
        let root = tempfile::tempdir().expect("temp swap test root");
        let output = root.path().join("published.png");
        let temp = root.path().join(".published.tmp");
        let moved_temp = root.path().join(".published-moved.tmp");
        let authority = CaptureOutputAuthority::new(&output, root.path())
            .expect("temp swap test authority should open");
        let mut file = open_temp_output(&temp).expect("trusted temp handle");
        std::io::Write::write_all(&mut file, b"trusted source").expect("trusted temp source");
        file.sync_all().expect("trusted temp sync");

        fs::rename(&temp, &moved_temp).expect("move trusted temp path");
        fs::write(&temp, b"attacker replacement").expect("attacker temp replacement");
        atomic_publish_temp(
            Some(temp.file_name().expect("temporary file name")),
            &authority,
            &file,
        )
        .expect("handle-relative publication");
        drop(file);

        assert_eq!(
            fs::read(&output).expect("published output"),
            b"trusted source"
        );
        assert_eq!(
            fs::read(&temp).expect("attacker temp replacement remains isolated"),
            b"attacker replacement"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_published_handle_cleanup_preserves_a_swapped_final_name() {
        let root = tempfile::tempdir().expect("published swap test root");
        let output = root.path().join("published.png");
        let temp = root.path().join(".published.tmp");
        let moved = root.path().join(".published-moved.png");
        let authority = CaptureOutputAuthority::new(&output, root.path())
            .expect("published swap test authority should open");
        let mut file = open_temp_output(&temp).expect("trusted temp handle");
        std::io::Write::write_all(&mut file, b"trusted source").expect("trusted source");
        file.sync_all().expect("trusted source sync");
        atomic_publish_temp(
            Some(temp.file_name().expect("temporary file name")),
            &authority,
            &file,
        )
        .expect("handle-relative publication");
        let published = PublishedOutput::from_handle(file).expect("published identity");

        fs::rename(&output, &moved).expect("attacker moves original final name");
        fs::write(&output, b"attacker replacement").expect("attacker replacement");
        assert!(
            published
                .verify_published_output_identity(&authority)
                .is_err(),
            "a moved original must not report the replacement as the published output"
        );
        published
            .delete_published_output_by_handle(&authority)
            .expect("handle cleanup should delete only the original inode");
        drop(published);

        assert_eq!(
            fs::read(&output).expect("replacement remains"),
            b"attacker replacement"
        );
        assert!(
            !moved.exists(),
            "the retained handle cleanup should remove the moved original"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_publish_temp_linux_rejects_named_temp_replacement() {
        use std::os::unix::fs::MetadataExt;

        let root = tempfile::tempdir().expect("publication test temp root");
        let output = root.path().join("published.png");
        let named = root.path().join(".published.tmp");
        let moved = root.path().join(".published-moved.tmp");
        let authority = CaptureOutputAuthority::new(&output, root.path())
            .expect("publication test authority should open");
        fs::write(&named, b"trusted named source").expect("named source");
        fs::rename(&named, &moved).expect("rename trusted named source");
        fs::write(&named, b"attacker replacement").expect("attacker replacement");

        let generation = CaptureGeneration::new();
        let lease = generation.begin();
        let (temp_name, mut file) = match open_temp_output_relative(
            &authority,
            &output,
            CaptureDeadline::from_now(Duration::from_secs(1)),
            &lease,
        ) {
            Ok(value) => value,
            Err(PreviewCaptureError::OutputFailed(message)) if message.contains("HOLD") => return,
            Err(error) => panic!("anonymous temporary publication failed: {error}"),
        };
        assert!(
            temp_name.is_none(),
            "Linux publication must never create a named temporary entry"
        );
        let trusted_inode = file.metadata().expect("anonymous source metadata").ino();
        std::io::Write::write_all(&mut file, b"trusted anonymous source")
            .expect("anonymous source");
        file.sync_all().expect("anonymous source sync");
        atomic_publish_temp(temp_name.as_deref(), &authority, &file)
            .expect("anonymous handle-relative publication");
        drop(file);

        assert_eq!(
            fs::read(&output).expect("published output"),
            b"trusted anonymous source"
        );
        assert_eq!(
            fs::read(&named).expect("attacker replacement remains isolated"),
            b"attacker replacement"
        );
        assert_eq!(
            fs::read(&moved).expect("renamed named source remains isolated"),
            b"trusted named source"
        );
        assert_eq!(
            fs::metadata(&output)
                .expect("published output metadata")
                .ino(),
            trusted_inode,
            "preview temporary inode identity changed during named-path replacement",
        );
    }
}

pub(crate) fn bounded_redacted_diagnostic(value: &str) -> String {
    let mut diagnostic = BoundedDiagnostic::new();
    diagnostic.write_bounded_text(value);
    diagnostic.rendered
}

#[doc(hidden)]
pub fn cleanup_output_after_deadline(
    output: &Path,
    primary: PreviewCaptureError,
    deadline: CaptureDeadline,
) -> PreviewCaptureError {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let authority = match CaptureOutputAuthority::new(output, parent) {
        Ok(authority) => Arc::new(authority),
        Err(error) => {
            return PreviewCaptureError::CleanupFailed(CleanupFailureContext::from_settlement(
                primary,
                "remove output",
                error,
            ));
        }
    };
    if in_capture_executor_worker() {
        let cleanup = authority.remove_output_relative();
        return match cleanup {
            Ok(()) => primary,
            Err(error) => PreviewCaptureError::CleanupFailed(
                CleanupFailureContext::from_settlement(primary, "remove output", error),
            ),
        };
    }
    let task = match spawn_cleanup_worker(move || authority.remove_output_relative()) {
        Ok(task) => task,
        Err(error) => {
            return PreviewCaptureError::CleanupFailed(CleanupFailureContext::from_settlement(
                primary,
                "remove output",
                error,
            ));
        }
    };
    match wait_for_worker_result(task, deadline) {
        Ok(()) => primary,
        Err(error) => PreviewCaptureError::CleanupFailed(CleanupFailureContext::from_settlement(
            primary,
            "remove output",
            error,
        )),
    }
}

/// Cleanup for a validated capture request.  The generic cleanup seam above
/// remains useful for isolated tests and callers without an authority, but a
/// live capture must revalidate the retained root/parent identities before it
/// ever resolves the output path for removal.  A substituted directory is
/// therefore left untouched and surfaced as a typed cleanup failure.
fn cleanup_authorized_output_after_deadline(
    authority: Arc<CaptureOutputAuthority>,
    primary: PreviewCaptureError,
    deadline: CaptureDeadline,
) -> PreviewCaptureError {
    let cleanup = move || authority.remove_output_relative();
    let cleanup_result = if in_capture_executor_worker() {
        cleanup()
    } else {
        match spawn_cleanup_worker(cleanup) {
            Ok(task) => wait_for_worker_result(task, deadline),
            Err(error) => Err(error),
        }
    };
    match cleanup_result {
        Ok(()) => primary,
        Err(error) => PreviewCaptureError::CleanupFailed(CleanupFailureContext::from_settlement(
            primary,
            "remove output",
            error,
        )),
    }
}

fn preserve_capture_error_after_authorized_cleanup(
    authority: Arc<CaptureOutputAuthority>,
    primary: PreviewCaptureError,
    deadline: CaptureDeadline,
) -> PreviewCaptureError {
    let settled = cleanup_authorized_output_after_deadline(authority, primary.clone(), deadline);
    if matches!(settled, PreviewCaptureError::CleanupFailed(_)) {
        settled
    } else {
        primary
    }
}

fn capture_executor() -> &'static CaptureExecutor {
    CAPTURE_EXECUTOR.get_or_init(|| {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<CaptureJob>(CAPTURE_EXECUTOR_QUEUE);
        let receiver = Arc::new(Mutex::new(receiver));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(CAPTURE_EXECUTOR_WORKERS);
        for index in 0..CAPTURE_EXECUTOR_WORKERS {
            let receiver = Arc::clone(&receiver);
            let shutdown_requested = Arc::clone(&shutdown_requested);
            let waiter = std::thread::Builder::new()
                .name(format!("devmanager-capture-executor-{index}"))
                .spawn(move || loop {
                    let job = receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv();
                    match job {
                        Ok(CaptureJob::Run(job)) => {
                            if !shutdown_requested.load(Ordering::Acquire) {
                                run_in_capture_executor_worker(job);
                            }
                        }
                        Ok(CaptureJob::Shutdown) => break,
                        Err(_) => break,
                    }
                })
                .expect("capture executor worker must be spawnable");
            workers.push(waiter);
        }
        CaptureExecutor {
            sender,
            workers: Mutex::new(workers),
            shutdown_requested,
        }
    })
}

/// Deterministically stop the fixed preview executor. This is intentionally
/// explicit because the process-global executor must remain available across
/// independent preview requests; callers that own the process shutdown point
/// can use this to join every worker and prove that no capture worker remains.
#[doc(hidden)]
pub fn shutdown_capture_executor() -> CaptureExecutorShutdownReport {
    shutdown_capture_executor_with_deadline(CAPTURE_EXECUTOR_SHUTDOWN_BUDGET)
}

/// A bounded shutdown never drops a live `JoinHandle`: workers that do not
/// observe cancellation before the deadline stay retained by the executor
/// and are reported as visible leaks. A later shutdown call can join them
/// after their blocking operation settles.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CaptureExecutorShutdownReport {
    pub workers_stopped: usize,
    pub workers_leaked: usize,
    pub shutdown_requested: bool,
}

impl std::fmt::Debug for CaptureExecutorShutdownReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureExecutorShutdownReport")
            .field("workers_stopped", &self.workers_stopped)
            .field("workers_leaked", &self.workers_leaked)
            .field("shutdown_requested", &self.shutdown_requested)
            .finish()
    }
}

impl std::fmt::Display for CaptureExecutorShutdownReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "CaptureExecutorShutdownReport(stopped={}, leaked={}, requested={})",
            self.workers_stopped, self.workers_leaked, self.shutdown_requested
        )
    }
}

#[doc(hidden)]
pub fn shutdown_capture_executor_with_deadline(budget: Duration) -> CaptureExecutorShutdownReport {
    let Some(executor) = CAPTURE_EXECUTOR.get() else {
        return CaptureExecutorShutdownReport {
            workers_stopped: 0,
            workers_leaked: 0,
            shutdown_requested: false,
        };
    };
    executor.shutdown_requested.store(true, Ordering::Release);
    let shutdown_deadline = Instant::now() + budget;
    let mut workers = executor
        .workers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if workers.is_empty() {
        return CaptureExecutorShutdownReport {
            workers_stopped: 0,
            workers_leaked: 0,
            shutdown_requested: true,
        };
    }
    // `send` could block behind the bounded queue. Try until the same
    // shutdown deadline while workers discard queued work after cancellation.
    let mut shutdown_messages = workers.len();
    while shutdown_messages > 0 && Instant::now() < shutdown_deadline {
        match executor.sender.try_send(CaptureJob::Shutdown) {
            Ok(()) => shutdown_messages -= 1,
            Err(TrySendError::Full(job)) => {
                let _ = job;
                std::thread::yield_now();
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }

    while Instant::now() < shutdown_deadline && workers.iter().any(|worker| !worker.is_finished()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut retained = Vec::with_capacity(workers.len());
    let mut workers_stopped = 0;
    let mut workers_leaked = 0;
    for worker in workers.drain(..) {
        if worker.is_finished() {
            let _ = worker.join();
            workers_stopped += 1;
        } else {
            // Retaining the handle is the ownership boundary: dropping it
            // would detach a worker that can still mutate capture state.
            retained.push(worker);
            workers_leaked += 1;
        }
    }
    *workers = retained;
    CaptureExecutorShutdownReport {
        workers_stopped,
        workers_leaked,
        shutdown_requested: true,
    }
}

fn spawn_capture_worker<F, T>(cleanup: F) -> Result<CaptureTask<T>, PreviewCaptureError>
where
    F: FnOnce() -> Result<T, PreviewCaptureError> + Send + 'static,
    T: Send + 'static,
{
    spawn_capture_worker_named("capture worker stopped without reporting a result", cleanup)
}

fn spawn_cleanup_worker<F, T>(cleanup: F) -> Result<CaptureTask<T>, PreviewCaptureError>
where
    F: FnOnce() -> Result<T, PreviewCaptureError> + Send + 'static,
    T: Send + 'static,
{
    spawn_capture_worker_named("cleanup worker stopped without reporting a result", cleanup)
}

fn spawn_capture_worker_named<F, T>(
    panic_message: &'static str,
    cleanup: F,
) -> Result<CaptureTask<T>, PreviewCaptureError>
where
    F: FnOnce() -> Result<T, PreviewCaptureError> + Send + 'static,
    T: Send + 'static,
{
    if capture_executor()
        .shutdown_requested
        .load(Ordering::Acquire)
    {
        return Err(PreviewCaptureError::CaptureFailed(
            "capture executor is shutting down".into(),
        ));
    }
    let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
    let job = CaptureJob::Run(Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup))
            .unwrap_or_else(|_| Err(PreviewCaptureError::CaptureFailed(panic_message.into())));
        let _ = result_sender.send(result);
    }));
    capture_executor()
        .sender
        .try_send(job)
        .map_err(|error| match error {
            TrySendError::Full(_) => PreviewCaptureError::CaptureFailed(
                "capture executor queue is full; capture failed closed".into(),
            ),
            TrySendError::Disconnected(_) => {
                PreviewCaptureError::CaptureFailed("capture executor is shut down".into())
            }
        })?;
    Ok(CaptureTask {
        receiver: result_receiver,
    })
}

fn wait_for_worker_result<T>(
    task: CaptureTask<T>,
    deadline: CaptureDeadline,
) -> Result<T, PreviewCaptureError>
where
    T: Send + 'static,
{
    match deadline.remaining() {
        Ok(remaining) => match task.receiver.recv_timeout(remaining) {
            Ok(result) => {
                let result = match deadline.remaining() {
                    Ok(_) => result,
                    Err(error) => Err(error),
                };
                result
            }
            Err(RecvTimeoutError::Timeout) => Err(PreviewCaptureError::DeadlineExceeded),
            Err(RecvTimeoutError::Disconnected) => Err(PreviewCaptureError::CaptureFailed(
                "cleanup worker stopped without reporting a result".into(),
            )),
        },
        Err(error) => Err(error),
    }
}

fn wait_for_capture_worker_result<T>(
    task: CaptureTask<T>,
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<T, PreviewCaptureError>
where
    T: Send + 'static,
{
    match wait_for_worker_result(task, deadline) {
        Err(PreviewCaptureError::DeadlineExceeded) => {
            lease.cancel();
            Err(PreviewCaptureError::DeadlineExceeded)
        }
        Ok(value) if lease.is_active() => Ok(value),
        Ok(_) | Err(PreviewCaptureError::CaptureCancelled) => {
            Err(PreviewCaptureError::CaptureCancelled)
        }
        Err(error) => Err(error),
    }
}

/// Execute one potentially blocking capture stage behind the same deadline
/// and generation fence used by the visible application.  Tests and platform
/// adapters use this seam to inject startup, frame, encode, and filesystem
/// stalls without ever blocking the caller or losing cleanup ownership.
#[doc(hidden)]
pub fn run_cancellable_stage<T, F>(
    deadline: CaptureDeadline,
    lease: CaptureLease,
    stage: F,
) -> Result<T, PreviewCaptureError>
where
    T: Send + 'static,
    F: FnOnce(CaptureDeadline, CaptureLease) -> Result<T, PreviewCaptureError> + Send + 'static,
{
    lease.check(deadline)?;
    let worker_lease = lease.clone();
    if in_capture_executor_worker() {
        if !worker_lease.is_active() {
            return Err(PreviewCaptureError::CaptureCancelled);
        }
        return stage(deadline, worker_lease);
    }
    let task = spawn_capture_worker(move || {
        if !worker_lease.is_active() {
            return Err(PreviewCaptureError::CaptureCancelled);
        }
        stage(deadline, worker_lease)
    })?;
    wait_for_capture_worker_result(task, deadline, &lease)
}

fn run_bounded_cleanup<F>(deadline: CaptureDeadline, cleanup: F)
where
    F: FnOnce() -> Result<(), PreviewCaptureError> + Send + 'static,
{
    if in_capture_executor_worker() {
        let _ = cleanup();
        return;
    }
    if let Ok(task) = spawn_cleanup_worker(cleanup) {
        let _ = wait_for_worker_result(task, deadline);
    }
}

/// Settles a first-frame result while keeping cleanup ownership if the
/// underlying stop/wait operation outlives the shared capture deadline.
#[doc(hidden)]
pub fn settle_capture_with_cleanup<T, F>(
    primary: Result<T, PreviewCaptureError>,
    deadline: CaptureDeadline,
    cleanup: F,
) -> Result<T, PreviewCaptureError>
where
    F: FnOnce(CaptureCleanupOperation) -> Result<(), PreviewCaptureError> + Send + 'static,
{
    let operation = if primary.is_ok() {
        CaptureCleanupOperation::Wait
    } else {
        CaptureCleanupOperation::Stop
    };
    let cleanup_result = if in_capture_executor_worker() {
        cleanup(operation)
    } else {
        match spawn_cleanup_worker(move || cleanup(operation)) {
            Ok(task) => wait_for_worker_result(task, deadline),
            Err(error) => Err(error),
        }
    };

    match (primary, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(PreviewCaptureError::CleanupFailed(
            CleanupFailureContext::from_settlement(
                PreviewCaptureError::CaptureFailed(
                    "a valid frame arrived but capture cleanup did not settle".into(),
                ),
                operation.as_str(),
                error,
            ),
        )),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(error)) => Err(PreviewCaptureError::CleanupFailed(
            CleanupFailureContext::from_settlement(primary, operation.as_str(), error),
        )),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NativeHwnd(pub isize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ValidatedWindow {
    pub hwnd: NativeHwnd,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CaptureReport {
    pub width: u32,
    pub height: u32,
    pub foreground_before: isize,
    pub foreground_after: isize,
}

macro_rules! opaque_capture_format {
    ($type:ty, $label:literal) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($label)
            }
        }

        impl std::fmt::Display for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($label)
            }
        }
    };
}

opaque_capture_format!(NativeHwnd, "NativeHwnd(<opaque>)");
opaque_capture_format!(ValidatedWindow, "ValidatedWindow(<opaque>)");
opaque_capture_format!(CaptureReport, "CaptureReport(<opaque>)");

#[doc(hidden)]
pub fn settle_capture_result(
    output: &Path,
    result: Result<CaptureReport, PreviewCaptureError>,
    deadline: CaptureDeadline,
) -> Result<CaptureReport, PreviewCaptureError> {
    match deadline.remaining() {
        Ok(_) => result,
        Err(error) => Err(cleanup_output_after_deadline(output, error, deadline)),
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum PreviewCaptureError {
    UnsupportedPlatform,
    InvalidHwnd,
    ForeignHwnd,
    InvalidWindowState { reason: &'static str },
    DeadlineExceeded,
    CaptureCancelled,
    CaptureClosed,
    CaptureFailed(String),
    ApplicationFailed(String),
    PngFailed(String),
    OutputAlreadyExists,
    OutputFailed(String),
    ForegroundChanged { before: isize, after: isize },
    CleanupFailed(CleanupFailureContext),
}

/// Opaque evidence that a capture failure and its cleanup failure were
/// observed by the same bounded settlement operation.
#[derive(Clone, PartialEq, Eq)]
pub struct CleanupFailureContext {
    primary: Box<PreviewCaptureError>,
    operation: &'static str,
    secondary: Box<PreviewCaptureError>,
}

impl CleanupFailureContext {
    fn from_settlement(
        primary: PreviewCaptureError,
        operation: &'static str,
        secondary: PreviewCaptureError,
    ) -> Self {
        Self {
            primary: Box::new(primary),
            operation,
            secondary: Box::new(secondary),
        }
    }

    pub fn primary(&self) -> &PreviewCaptureError {
        &self.primary
    }

    pub fn operation(&self) -> &'static str {
        self.operation
    }

    pub fn secondary(&self) -> &PreviewCaptureError {
        &self.secondary
    }
}

impl std::fmt::Debug for CleanupFailureContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CleanupFailureContext")
            .field(
                "message",
                &bounded_redacted_diagnostic(&format!(
                    "{}; cleanup {} failed: {}",
                    self.primary, self.operation, self.secondary
                )),
            )
            .finish()
    }
}

impl std::fmt::Display for CleanupFailureContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CleanupFailureContext(<opaque>)")
    }
}

struct BoundedDiagnostic {
    rendered: String,
    truncated: bool,
}

impl BoundedDiagnostic {
    fn new() -> Self {
        Self {
            rendered: String::with_capacity(MAX_CLEANUP_DIAGNOSTIC_BYTES),
            truncated: false,
        }
    }

    fn truncate(&mut self) {
        if self.truncated {
            return;
        }

        let payload_limit =
            MAX_CLEANUP_DIAGNOSTIC_BYTES.saturating_sub(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER.len());
        let boundary = utf8_prefix_len(&self.rendered, payload_limit);
        self.rendered.truncate(boundary);
        self.rendered.push_str(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER);
        self.truncated = true;
    }

    fn write_bounded_text(&mut self, value: &str) {
        if self.truncated {
            return;
        }

        let payload_available = MAX_CLEANUP_DIAGNOSTIC_BYTES
            .saturating_sub(self.rendered.len())
            .saturating_sub(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER.len());
        let value = crate::ui::components::interaction::redact_sensitive_text(value);
        if value.len() > payload_available {
            let boundary = utf8_prefix_len(&value, payload_available);
            self.rendered.push_str(&value[..boundary]);
            self.truncate();
        } else {
            self.rendered.push_str(&value);
        }
    }
}

impl std::fmt::Write for BoundedDiagnostic {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.truncated || value.is_empty() {
            return Ok(());
        }

        let available = MAX_CLEANUP_DIAGNOSTIC_BYTES.saturating_sub(self.rendered.len());
        if value.len() <= available {
            self.rendered.push_str(value);
            return Ok(());
        }

        let payload_available = MAX_CLEANUP_DIAGNOSTIC_BYTES
            .saturating_sub(self.rendered.len())
            .saturating_sub(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER.len());
        let boundary = utf8_prefix_len(value, payload_available);
        self.rendered.push_str(&value[..boundary]);
        self.truncate();
        Ok(())
    }
}

fn utf8_prefix_len(value: &str, max_bytes: usize) -> usize {
    let mut boundary = value.len().min(max_bytes);
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn render_capture_error(
    error: &PreviewCaptureError,
    diagnostic: &mut BoundedDiagnostic,
    depth: usize,
) {
    if diagnostic.truncated {
        return;
    }
    if depth >= MAX_CLEANUP_DIAGNOSTIC_DEPTH {
        diagnostic.truncate();
        return;
    }

    match error {
        PreviewCaptureError::UnsupportedPlatform => {
            let _ =
                diagnostic.write_str("a visible Windows desktop is required for native capture");
        }
        PreviewCaptureError::InvalidHwnd => {
            let _ = diagnostic.write_str("the preview HWND is invalid or closed");
        }
        PreviewCaptureError::ForeignHwnd => {
            let _ = diagnostic.write_str("the preview HWND is owned by another process");
        }
        PreviewCaptureError::InvalidWindowState { reason } => {
            let _ = diagnostic.write_str("the preview HWND is unavailable: ");
            diagnostic.write_bounded_text(reason);
        }
        PreviewCaptureError::DeadlineExceeded => {
            let _ = diagnostic.write_str("no valid frame arrived before the fixed deadline");
        }
        PreviewCaptureError::CaptureCancelled => {
            let _ = diagnostic.write_str("the preview capture was cancelled before publication");
        }
        PreviewCaptureError::CaptureClosed => {
            let _ = diagnostic.write_str("the preview capture item closed before a frame arrived");
        }
        PreviewCaptureError::CaptureFailed(message) => {
            let _ = diagnostic.write_str("Windows Graphics Capture failed: ");
            diagnostic.write_bounded_text(message);
        }
        PreviewCaptureError::ApplicationFailed(message) => {
            let _ = diagnostic.write_str("GPUI preview application failed: ");
            diagnostic.write_bounded_text(message);
        }
        PreviewCaptureError::PngFailed(message) => {
            let _ = diagnostic.write_str("PNG encoding failed: ");
            diagnostic.write_bounded_text(message);
        }
        PreviewCaptureError::OutputAlreadyExists => {
            let _ = diagnostic.write_str("refusing to overwrite an existing PNG output");
        }
        PreviewCaptureError::OutputFailed(message) => {
            let _ = diagnostic.write_str("PNG output failed: ");
            diagnostic.write_bounded_text(message);
        }
        PreviewCaptureError::ForegroundChanged { before, after } => {
            let _ = (before, after);
            let _ = diagnostic.write_str("foreground window changed during capture");
        }
        PreviewCaptureError::CleanupFailed(context) => {
            render_capture_error(context.primary(), diagnostic, depth + 1);
            let _ = write!(diagnostic, "; cleanup {} failed: ", context.operation());
            render_capture_error(context.secondary(), diagnostic, depth + 1);
        }
    }
}

impl std::fmt::Display for PreviewCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut diagnostic = BoundedDiagnostic::new();
        render_capture_error(self, &mut diagnostic, 0);
        f.write_str(&diagnostic.rendered)
    }
}

impl std::fmt::Debug for PreviewCaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewCaptureError")
            .field("message", &self.to_string())
            .finish()
    }
}

impl std::error::Error for PreviewCaptureError {}

pub fn receive_first_frame<T>(
    receiver: Receiver<T>,
    deadline: CaptureDeadline,
) -> Result<T, PreviewCaptureError> {
    match receiver.recv_timeout(deadline.remaining()?) {
        Ok(frame) => {
            deadline.remaining()?;
            Ok(frame)
        }
        Err(RecvTimeoutError::Timeout) => Err(PreviewCaptureError::DeadlineExceeded),
        Err(RecvTimeoutError::Disconnected) => Err(PreviewCaptureError::CaptureClosed),
    }
}

#[doc(hidden)]
pub fn receive_first_frame_with_lease<T>(
    receiver: Receiver<T>,
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<T, PreviewCaptureError> {
    if let Err(error) = lease.check(deadline) {
        lease.cancel();
        return Err(error);
    }
    let frame = match receive_first_frame(receiver, deadline) {
        Ok(frame) => frame,
        Err(error) => {
            lease.cancel();
            return Err(error);
        }
    };
    if let Err(error) = lease.check(deadline) {
        lease.cancel();
        return Err(error);
    }
    Ok(frame)
}

#[doc(hidden)]
pub fn encode_bgra_png_atomic(
    output: &Path,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
) -> Result<(), PreviewCaptureError> {
    let lease = CaptureGeneration::new().begin();
    encode_bgra_png_atomic_with_lease(output, width, height, bgra, deadline, &lease)
}

#[doc(hidden)]
pub fn encode_bgra_png_atomic_with_root(
    output: &Path,
    trusted_root: &Path,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
) -> Result<(), PreviewCaptureError> {
    let authority = Arc::new(CaptureOutputAuthority::new(output, trusted_root)?);
    let lease = CaptureGeneration::new().begin();
    encode_bgra_png_atomic_owned(
        output.to_path_buf(),
        width,
        height,
        bgra.to_vec(),
        deadline,
        lease,
        Some(authority),
        None,
    )
}

#[doc(hidden)]
pub fn encode_bgra_png_atomic_with_lease(
    output: &Path,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<(), PreviewCaptureError> {
    lease.check(deadline)?;
    deadline.remaining()?;
    let bgra = bgra.to_vec();
    lease.check(deadline)?;
    encode_bgra_png_atomic_owned(
        output.to_path_buf(),
        width,
        height,
        bgra,
        deadline,
        lease.clone(),
        None,
        None,
    )
}

#[doc(hidden)]
pub fn encode_bgra_png_atomic_with_authority(
    authority: Arc<CaptureOutputAuthority>,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<(), PreviewCaptureError> {
    lease.check(deadline)?;
    encode_bgra_png_atomic_owned(
        authority.output_path(),
        width,
        height,
        bgra.to_vec(),
        deadline,
        lease.clone(),
        Some(authority),
        None,
    )
}

#[cfg(windows)]
pub(crate) fn encode_bgra_png_atomic_with_authority_and_publication<F>(
    authority: Arc<CaptureOutputAuthority>,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
    lease: &CaptureLease,
    publication: F,
) -> Result<(), PreviewCaptureError>
where
    F: FnOnce() + Send + 'static,
{
    lease.check(deadline)?;
    encode_bgra_png_atomic_owned(
        authority.output_path(),
        width,
        height,
        bgra.to_vec(),
        deadline,
        lease.clone(),
        Some(authority),
        Some(Box::new(publication)),
    )
}

fn encode_bgra_png_atomic_owned(
    output: PathBuf,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    deadline: CaptureDeadline,
    lease: CaptureLease,
    authority: Option<Arc<CaptureOutputAuthority>>,
    publication: Option<PublicationCallback>,
) -> Result<(), PreviewCaptureError> {
    lease.check(deadline)?;
    if in_capture_executor_worker() {
        return encode_bgra_png_atomic_sync(
            &output,
            width,
            height,
            &bgra,
            deadline,
            &lease,
            authority,
            publication,
        );
    }
    let lease_for_worker = lease.clone();
    let task = spawn_capture_worker(move || {
        encode_bgra_png_atomic_sync(
            &output,
            width,
            height,
            &bgra,
            deadline,
            &lease_for_worker,
            authority,
            publication,
        )
    })?;
    wait_for_capture_worker_result(task, deadline, &lease)
}

fn encode_bgra_png_atomic_sync(
    output: &Path,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
    lease: &CaptureLease,
    authority: Option<Arc<CaptureOutputAuthority>>,
    mut publication: Option<PublicationCallback>,
) -> Result<(), PreviewCaptureError> {
    lease.check(deadline)?;
    if authority.is_none() {
        reject_reparse_ancestors(output)?;
    }
    let expected_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| PreviewCaptureError::PngFailed("frame dimensions overflowed".into()))?;
    if width == 0 || height == 0 || bgra.len() != expected_bytes {
        return Err(PreviewCaptureError::PngFailed(
            "BGRA frame dimensions do not match its byte length".into(),
        ));
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            PreviewCaptureError::OutputFailed("PNG output has no parent directory".into())
        })?;
    if authority.is_none() {
        reject_reparse_ancestors(parent)?;
        fs::create_dir_all(parent)
            .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
    }
    let authority = match authority {
        Some(authority) => authority,
        None => Arc::new(CaptureOutputAuthority::new(output, parent)?),
    };
    authority.verify_reopened()?;
    lease.check(deadline)?;

    let (temp_name, file) = open_temp_output_relative(&authority, output, deadline, lease)?;
    let cleanup = TempCleanupState::default();
    let mut temp = TempOutput::new(temp_name.clone(), Arc::clone(&authority), cleanup.clone());
    let result = (|| -> Result<(), PreviewCaptureError> {
        let mut file = file;
        lease.check(deadline)?;

        let mut rgba = Vec::with_capacity(expected_bytes);
        for pixel in bgra.chunks_exact(4) {
            lease.check(deadline)?;
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }

        {
            lease.check(deadline)?;
            let encoder = image::codecs::png::PngEncoder::new(&mut file);
            encoder
                .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|error| PreviewCaptureError::PngFailed(error.to_string()))?;
        }

        lease.check(deadline)?;
        file.sync_all()
            .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
        lease.check(deadline)?;
        lease.publish_with(deadline, || {
            let published = PublishedOutput::from_handle(file)?;
            let publication_result = atomic_publish_temp(
                temp_name.as_deref(),
                &authority,
                &published.final_output_handle,
            )
            .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
            temp.final_output = Some(published);
            temp.renamed = true;
            temp.temp_removed = matches!(&publication_result, AtomicPublishOutcome::Published);
            temp.final_output
                .as_ref()
                .expect("published output retained before identity verification")
                .verify_published_output_identity(&authority)?;
            if let Some(callback) = publication.take() {
                callback();
            }
            // The callback may publish receipt metadata and external actors
            // may race the final name while it runs.  Revalidate immediately
            // before the generation commit can report success.
            temp.final_output
                .as_ref()
                .expect("published output retained through commit")
                .verify_published_output_identity(&authority)?;
            Ok(())
        })?;
        temp.committed = true;
        Ok(())
    })();
    drop(temp);
    settle_temp_cleanup_result(result, &cleanup)
}

#[cfg(test)]
fn open_temp_output(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options.access_mode(
            windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0
                | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0
                | windows::Win32::Storage::FileSystem::DELETE.0,
        );
        options.share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);
        options.custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    options.open(path)
}

fn open_temp_output_relative(
    authority: &CaptureOutputAuthority,
    output: &Path,
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<(Option<OsString>, std::fs::File), PreviewCaptureError> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let _ = output;
        lease.check(deadline)?;
        let parent = authority.reopen_parent_for_publication()?;
        let directory = CString::new(".").expect("static anonymous temp directory");
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                directory.as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            let message = match error.raw_os_error() {
                Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) => {
                    "anonymous Linux preview temporary inodes are unavailable; visual capture HOLD"
                }
                _ => "anonymous Linux preview temporary inode could not be created",
            };
            return Err(PreviewCaptureError::OutputFailed(message.into()));
        }
        return Ok((
            None,
            std::fs::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
        ));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = (authority, output, deadline, lease);
        return Err(PreviewCaptureError::OutputFailed(
            "anonymous handle-relative preview temporary inodes are unsupported; visual capture HOLD".into(),
        ));
    }

    #[cfg(not(unix))]
    let stem = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("preview");
    #[cfg(not(unix))]
    for _ in 0..32 {
        lease.check(deadline)?;
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = next_temp_name(stem, counter);
        #[cfg(windows)]
        {
            let parent = authority.reopen_parent_for_publication()?;
            match open_relative_windows(&parent, &name) {
                Ok(file) => return Ok((Some(name), file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(PreviewCaptureError::OutputFailed(format!(
                        "preview temp open failed: {error}"
                    )));
                }
            }
        }
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;

            let name_c = CString::new(name.as_os_str().as_bytes()).map_err(|_| {
                PreviewCaptureError::OutputFailed("preview temp name contains NUL".into())
            })?;
            let parent = authority.reopen_parent_for_publication()?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(PreviewCaptureError::OutputFailed(format!(
                    "preview temp open failed: {error}"
                )));
            }
            return Ok((
                Some(name),
                std::fs::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
            ));
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = name;
            return Err(PreviewCaptureError::OutputFailed(
                "handle-relative preview temp creation is unsupported on this platform".into(),
            ));
        }
    }
    Err(PreviewCaptureError::OutputFailed(
        "could not allocate a unique temporary PNG entry".into(),
    ))
}

fn next_temp_name(stem: &str, counter: usize) -> OsString {
    OsString::from(format!(".{stem}.{counter}.tmp"))
}

/// Publish a fully synced temporary PNG without following a reparse point at
/// the final file boundary. Windows uses the open temporary-file handle and a
/// no-follow parent handle. Linux uses the held parent descriptor plus the
/// open inode (`linkat(AT_EMPTY_PATH)`) so a swapped temporary path cannot
/// replace the source. Other Unix platforms fail closed rather than resolving
/// an absolute parent path after validation.
#[allow(dead_code)]
enum AtomicPublishOutcome {
    Published,
}

fn atomic_publish_temp(
    temp: Option<&std::ffi::OsStr>,
    authority: &CaptureOutputAuthority,
    file: &std::fs::File,
) -> std::io::Result<AtomicPublishOutcome> {
    #[cfg(windows)]
    {
        return atomic_publish_temp_windows(temp, authority, file);
    }
    #[cfg(not(windows))]
    {
        #[cfg(unix)]
        {
            return atomic_publish_temp_unix(temp, authority, file);
        }
        #[cfg(not(unix))]
        {
            let _ = (temp, authority, file);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "handle-relative preview publication is unsupported on this platform",
            ));
        }
    }
}

#[cfg(unix)]
fn atomic_publish_temp_unix(
    _temp: Option<&std::ffi::OsStr>,
    authority: &CaptureOutputAuthority,
    file: &std::fs::File,
) -> std::io::Result<AtomicPublishOutcome> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let parent = authority.reopen_parent_for_publication().map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, error.to_string())
    })?;
    let output_name = CString::new(authority.output_name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preview output name is invalid",
        )
    })?;

    #[cfg(target_os = "linux")]
    {
        // Link the held inode directly into the held directory. This is the
        // no-follow equivalent of renaming the open temp handle and refuses
        // to replace a destination created after validation.
        let empty = [0_i8];
        let result = unsafe {
            libc::linkat(
                file.as_raw_fd(),
                empty.as_ptr(),
                parent.as_raw_fd(),
                output_name.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(code) if
                code == libc::EPERM || code == libc::EOPNOTSUPP || code == libc::EINVAL)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "anonymous Linux preview publication is unavailable; visual capture HOLD",
                ));
            }
            return Err(error);
        }
        return Ok(AtomicPublishOutcome::Published);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "handle-relative preview publication is unsupported on this Unix platform",
        ))
    }
}

#[cfg(windows)]
fn atomic_publish_temp_windows(
    _temp: Option<&std::ffi::OsStr>,
    authority: &CaptureOutputAuthority,
    file: &std::fs::File,
) -> std::io::Result<AtomicPublishOutcome> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    // The Win32 wrapper rejects a non-null RootDirectory on some supported
    // Windows builds even though the native contract accepts it.  Calling the
    // native file-information boundary keeps the destination directory
    // handle-relative and avoids falling back to a re-resolved path.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut NativeIoStatusBlock,
            file_information: *const std::ffi::c_void,
            length: u32,
            file_information_class: i32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    let parent_handle = authority.reopen_parent_for_publication().map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, error.to_string())
    })?;
    let mut file_name: Vec<u16> = authority.output_name.encode_wide().collect();
    // The length excludes the terminator, but the native structure still
    // expects a NUL-terminated relative name in its flexible array member.
    file_name.push(0);

    let file_handle = HANDLE(file.as_raw_handle());

    let bytes = file_name
        .len()
        .saturating_sub(1)
        .checked_mul(2)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long")
        })?;
    let header = std::mem::size_of::<windows::Win32::Storage::FileSystem::FILE_RENAME_INFO>();
    let total = header.checked_add(bytes).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long")
    })?;
    let word_count = total
        .checked_add(std::mem::align_of::<u64>() - 1)
        .map(|size| size / std::mem::align_of::<u64>())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long")
        })?;
    let mut rename = vec![0_u64; word_count];
    let info = rename.as_mut_ptr() as *mut FILE_RENAME_INFO;
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = HANDLE(parent_handle.as_raw_handle());
        (*info).FileNameLength = u32::try_from(bytes).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long")
        })?;
        std::ptr::copy_nonoverlapping(
            file_name.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            file_name.len(),
        );
        let mut io_status = NativeIoStatusBlock {
            status: 0,
            information: 0,
        };
        let status = NtSetInformationFile(
            file_handle,
            &mut io_status,
            info.cast(),
            u32::try_from(total).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long")
            })?,
            10,
        );
        if status < 0 {
            let error = RtlNtStatusToDosError(status);
            return Err(std::io::Error::from_raw_os_error(
                i32::try_from(error).unwrap_or(i32::MAX),
            ));
        }
    }
    Ok(AtomicPublishOutcome::Published)
}

fn reject_reparse_ancestors(path: &Path) -> Result<(), PreviewCaptureError> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            let metadata = fs::symlink_metadata(&current).map_err(|_error| {
                PreviewCaptureError::OutputFailed("output path could not be inspected".into())
            })?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes()
                    & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
                    != 0
                {
                    return Err(PreviewCaptureError::OutputFailed(
                        "output path contains a reparse-point ancestor".into(),
                    ));
                }
            }
            #[cfg(not(windows))]
            if metadata.file_type().is_symlink() {
                return Err(PreviewCaptureError::OutputFailed(
                    "output path contains a symbolic-link ancestor".into(),
                ));
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

struct TempCleanupFailure {
    operation: &'static str,
    error: PreviewCaptureError,
}

#[derive(Clone, Default)]
struct TempCleanupState {
    failures: Arc<Mutex<Vec<TempCleanupFailure>>>,
}

impl TempCleanupState {
    fn record_cleanup_failure(&self, operation: &'static str, error: PreviewCaptureError) {
        let mut failures = self
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failures.len() < 2 {
            failures.push(TempCleanupFailure { operation, error });
        }
    }

    fn take(&self) -> Vec<TempCleanupFailure> {
        let mut failures = self
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *failures)
    }
}

fn settle_temp_cleanup_result(
    mut result: Result<(), PreviewCaptureError>,
    state: &TempCleanupState,
) -> Result<(), PreviewCaptureError> {
    for failure in state.take() {
        let primary = match result {
            Ok(()) => {
                PreviewCaptureError::CaptureFailed("capture output cleanup did not settle".into())
            }
            Err(error) => error,
        };
        result = Err(PreviewCaptureError::CleanupFailed(
            CleanupFailureContext::from_settlement(primary, failure.operation, failure.error),
        ));
    }
    result
}

struct TempOutput {
    temp_name: Option<OsString>,
    authority: Arc<CaptureOutputAuthority>,
    cleanup: TempCleanupState,
    final_output: Option<PublishedOutput>,
    renamed: bool,
    temp_removed: bool,
    committed: bool,
}

impl TempOutput {
    fn new(
        temp_name: Option<OsString>,
        authority: Arc<CaptureOutputAuthority>,
        cleanup: TempCleanupState,
    ) -> Self {
        let temp_removed = temp_name.is_none();
        Self {
            temp_name,
            authority,
            cleanup,
            final_output: None,
            renamed: false,
            temp_removed,
            committed: false,
        }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let authority_valid = match self.authority.verify_reopened() {
            Ok(()) => true,
            Err(error) => {
                self.cleanup
                    .record_cleanup_failure("verify output authority", error);
                false
            }
        };
        if authority_valid && !self.temp_removed {
            if let Some(temp_name) = &self.temp_name {
                if let Err(error) = self.authority.remove_temp_relative(temp_name) {
                    self.cleanup
                        .record_cleanup_failure("remove temporary output", error);
                } else {
                    self.temp_removed = true;
                }
            } else {
                self.temp_removed = true;
            }
        }
        if self.renamed && !self.committed {
            if let Some(published) = self.final_output.as_ref() {
                if let Err(error) = published.delete_published_output_by_handle(&self.authority) {
                    self.cleanup
                        .record_cleanup_failure("remove published output", error);
                }
            } else {
                self.cleanup.record_cleanup_failure(
                    "remove published output",
                    PreviewCaptureError::OutputFailed("output residue is unresolved".into()),
                );
            }
        }
    }
}

#[cfg(windows)]
mod windows_capture_impl {
    use super::*;
    use gpui::{
        px, size, AppContext, Bounds, Window, WindowBackgroundAppearance, WindowBounds, WindowKind,
        WindowOptions,
    };
    use raw_window_handle::RawWindowHandle;
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::mpsc::{self, SyncSender};
    use std::sync::Arc;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
        GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetWindowLongPtrW,
        SetWindowPos, ShowWindow, GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
    };
    use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmGetWindowAttribute(
            hwnd: HWND,
            attribute: u32,
            value: *mut c_void,
            value_size: u32,
        ) -> i32;
    }

    fn extended_frame_size(hwnd: HWND) -> Option<(i32, i32)> {
        const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
        let mut bounds = RECT::default();
        let status = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut bounds as *mut RECT).cast(),
                u32::try_from(std::mem::size_of::<RECT>()).ok()?,
            )
        };
        (status == 0).then_some((
            bounds.right.saturating_sub(bounds.left),
            bounds.bottom.saturating_sub(bounds.top),
        ))
    }

    #[derive(Debug)]
    struct HandlerError(PreviewCaptureError);

    impl std::fmt::Display for HandlerError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl std::error::Error for HandlerError {}

    impl HandlerError {
        fn into_capture_error(self) -> PreviewCaptureError {
            self.0
        }
    }

    #[derive(Debug)]
    struct CapturedFrame {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    }

    struct FirstFrameHandler {
        sender: SyncSender<Result<CapturedFrame, HandlerError>>,
        hwnd: NativeHwnd,
        lease: CaptureLease,
    }

    impl FirstFrameHandler {
        fn send_error(&self, error: PreviewCaptureError, capture_control: InternalCaptureControl) {
            if self.lease.is_active() {
                let _ = self.sender.try_send(Err(HandlerError(error)));
            }
            capture_control.stop();
        }
    }

    #[allow(dead_code)]
    struct CaptureNotificationTask(gpui::Task<()>);

    impl gpui::Global for CaptureNotificationTask {}

    #[allow(dead_code)]
    struct CaptureDeadlineTask(gpui::Task<()>);

    impl gpui::Global for CaptureDeadlineTask {}

    fn store_capture_result(
        slot: &Arc<Mutex<Option<Result<CaptureReport, PreviewCaptureError>>>>,
        lease: &CaptureLease,
        result: Result<CaptureReport, PreviewCaptureError>,
    ) {
        if !lease.is_active() {
            return;
        }
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(result);
        }
    }

    fn store_capture_result_after_commit(
        slot: &Arc<Mutex<Option<Result<CaptureReport, PreviewCaptureError>>>>,
        result: Result<CaptureReport, PreviewCaptureError>,
    ) {
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(result);
        }
    }

    impl GraphicsCaptureApiHandler for FirstFrameHandler {
        type Flags = (
            SyncSender<Result<CapturedFrame, HandlerError>>,
            NativeHwnd,
            CaptureLease,
        );
        type Error = HandlerError;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self {
                sender: ctx.flags.0,
                hwnd: ctx.flags.1,
                lease: ctx.flags.2,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            if !self.lease.is_active() {
                capture_control.stop();
                return Ok(());
            }
            if let Err(error) = validate_native_window(self.hwnd) {
                self.send_error(error, capture_control);
                return Ok(());
            }

            let mut buffer = match frame.buffer() {
                Ok(buffer) => buffer,
                Err(error) => {
                    self.send_error(
                        PreviewCaptureError::CaptureFailed(error.to_string()),
                        capture_control,
                    );
                    return Ok(());
                }
            };
            let width = buffer.width();
            let height = buffer.height();
            let bgra = match buffer.as_nopadding_buffer() {
                Ok(buffer) => buffer.to_vec(),
                Err(error) => {
                    self.send_error(
                        PreviewCaptureError::CaptureFailed(error.to_string()),
                        capture_control,
                    );
                    return Ok(());
                }
            };
            if width != PREVIEW_WINDOW_WIDTH as u32 || height != PREVIEW_WINDOW_HEIGHT as u32 {
                self.send_error(
                    PreviewCaptureError::InvalidWindowState {
                        reason: "capture frame dimensions do not match the 640x360 contract",
                    },
                    capture_control,
                );
                return Ok(());
            }
            if width == 0 || height == 0 {
                self.send_error(
                    PreviewCaptureError::CaptureFailed(
                        "Windows Graphics Capture returned an empty frame".into(),
                    ),
                    capture_control,
                );
                return Ok(());
            }

            if !self.lease.is_active() {
                capture_control.stop();
                return Ok(());
            }
            let _ = self.sender.try_send(Ok(CapturedFrame {
                width,
                height,
                bgra,
            }));
            capture_control.stop();
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            if self.lease.is_active() {
                let _ = self
                    .sender
                    .try_send(Err(HandlerError(PreviewCaptureError::CaptureClosed)));
            }
            Ok(())
        }
    }

    struct CaptureControlGuard {
        control: Option<CaptureControl<FirstFrameHandler, HandlerError>>,
        deadline: CaptureDeadline,
    }

    impl CaptureControlGuard {
        fn new(
            control: CaptureControl<FirstFrameHandler, HandlerError>,
            deadline: CaptureDeadline,
        ) -> Self {
            Self {
                control: Some(control),
                deadline,
            }
        }

        fn cleanup(
            mut self,
            operation: CaptureCleanupOperation,
        ) -> Result<(), PreviewCaptureError> {
            let control = self.control.take().ok_or_else(|| {
                PreviewCaptureError::CaptureFailed("capture control was already consumed".into())
            })?;
            match operation {
                CaptureCleanupOperation::Stop => control.stop().map_err(map_control_error),
                CaptureCleanupOperation::Wait => control.wait().map_err(map_control_error),
            }
        }
    }

    impl Drop for CaptureControlGuard {
        fn drop(&mut self) {
            let Some(control) = self.control.take() else {
                return;
            };
            let deadline = self.deadline;
            run_bounded_cleanup(deadline, move || {
                control.stop().map_err(|error| {
                    PreviewCaptureError::CaptureFailed(format!(
                        "capture guard drop cleanup: {error}"
                    ))
                })
            });
        }
    }

    struct ActiveCaptureGuard;

    impl ActiveCaptureGuard {
        fn new() -> Self {
            ACTIVE_CAPTURE_THREADS.fetch_add(1, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for ActiveCaptureGuard {
        fn drop(&mut self) {
            ACTIVE_CAPTURE_THREADS.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn map_graphics_capture_error(
        error: windows_capture::capture::GraphicsCaptureApiError<HandlerError>,
    ) -> PreviewCaptureError {
        use windows_capture::capture::GraphicsCaptureApiError;

        match error {
            GraphicsCaptureApiError::NewHandlerError(error)
            | GraphicsCaptureApiError::FrameHandlerError(error) => error.into_capture_error(),
            error => PreviewCaptureError::CaptureFailed(error.to_string()),
        }
    }

    fn map_control_error(
        error: windows_capture::capture::CaptureControlError<HandlerError>,
    ) -> PreviewCaptureError {
        use windows_capture::capture::CaptureControlError;

        match error {
            CaptureControlError::StoppedHandlerError(error) => error.into_capture_error(),
            CaptureControlError::GraphicsCaptureApiError(error) => {
                map_graphics_capture_error(error)
            }
            error => PreviewCaptureError::CaptureFailed(error.to_string()),
        }
    }

    struct StartedCapture {
        control: CaptureControlGuard,
        active: ActiveCaptureGuard,
    }

    type CaptureSettings = Settings<
        (
            SyncSender<Result<CapturedFrame, HandlerError>>,
            NativeHwnd,
            CaptureLease,
        ),
        windows_capture::window::Window,
    >;

    fn start_capture_sync(
        settings: CaptureSettings,
        deadline: CaptureDeadline,
        lease: &CaptureLease,
    ) -> Result<StartedCapture, PreviewCaptureError> {
        if !lease.is_active() {
            return Err(PreviewCaptureError::CaptureCancelled);
        }
        let active = ActiveCaptureGuard::new();
        let result =
            FirstFrameHandler::start_free_threaded(settings).map_err(map_graphics_capture_error);
        match result {
            Ok(control) if lease.is_active() => Ok(StartedCapture {
                control: CaptureControlGuard::new(control, deadline),
                active,
            }),
            Ok(control) => {
                drop(CaptureControlGuard::new(control, deadline));
                drop(active);
                Err(PreviewCaptureError::CaptureCancelled)
            }
            Err(error) => {
                drop(active);
                Err(error)
            }
        }
    }

    fn start_capture(
        settings: CaptureSettings,
        deadline: CaptureDeadline,
        lease: &CaptureLease,
    ) -> Result<StartedCapture, PreviewCaptureError> {
        lease.check(deadline)?;
        if in_capture_executor_worker() {
            return start_capture_sync(settings, deadline, lease);
        }
        let lease_for_worker = lease.clone();
        let task = spawn_capture_worker(move || {
            if !lease_for_worker.is_active() {
                return Err(PreviewCaptureError::CaptureCancelled);
            }
            let active = ActiveCaptureGuard::new();
            let result = FirstFrameHandler::start_free_threaded(settings)
                .map_err(map_graphics_capture_error);
            match result {
                Ok(control) => {
                    if !lease_for_worker.is_active() {
                        drop(CaptureControlGuard::new(control, deadline));
                        drop(active);
                        return Err(PreviewCaptureError::CaptureCancelled);
                    }
                    Ok(StartedCapture {
                        control: CaptureControlGuard::new(control, deadline),
                        active,
                    })
                }
                Err(error) => {
                    drop(active);
                    Err(error)
                }
            }
        })?;
        match wait_for_worker_result(task, deadline) {
            Ok(started) => {
                if let Err(error) = deadline.remaining() {
                    drop(started);
                    lease.cancel();
                    Err(error)
                } else if lease.is_active() {
                    Ok(started)
                } else {
                    drop(started);
                    Err(PreviewCaptureError::CaptureCancelled)
                }
            }
            Err(PreviewCaptureError::DeadlineExceeded) => {
                lease.cancel();
                Err(PreviewCaptureError::DeadlineExceeded)
            }
            Err(error) => {
                lease.cancel();
                Err(error)
            }
        }
    }

    pub fn foreground_hwnd() -> isize {
        unsafe { GetForegroundWindow().0 as isize }
    }

    pub fn hwnd_from_gpui_window(window: &Window) -> Result<NativeHwnd, PreviewCaptureError> {
        let handle = raw_window_handle::HasWindowHandle::window_handle(window)
            .map_err(|error| PreviewCaptureError::ApplicationFailed(error.to_string()))?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(NativeHwnd(handle.hwnd.get())),
            _ => Err(PreviewCaptureError::UnsupportedPlatform),
        }
    }

    pub fn validate_native_window(
        hwnd: NativeHwnd,
    ) -> Result<ValidatedWindow, PreviewCaptureError> {
        let hwnd = hwnd.as_hwnd();
        if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            return Err(PreviewCaptureError::InvalidHwnd);
        }

        let mut process_id = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if process_id == 0 {
            return Err(PreviewCaptureError::InvalidHwnd);
        }
        if process_id != unsafe { GetCurrentProcessId() } {
            return Err(PreviewCaptureError::ForeignHwnd);
        }
        if !unsafe { IsWindowVisible(hwnd).as_bool() } {
            return Err(PreviewCaptureError::InvalidWindowState { reason: "hidden" });
        }
        if unsafe { IsIconic(hwnd).as_bool() } {
            return Err(PreviewCaptureError::InvalidWindowState {
                reason: "minimized",
            });
        }

        let mut window_rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut window_rect) }.map_err(|_| {
            PreviewCaptureError::InvalidWindowState {
                reason: "no window bounds",
            }
        })?;
        let mut client_rect = RECT::default();
        unsafe { GetClientRect(hwnd, &mut client_rect) }.map_err(|_| {
            PreviewCaptureError::InvalidWindowState {
                reason: "no client bounds",
            }
        })?;
        let width = window_rect.right.saturating_sub(window_rect.left);
        let height = window_rect.bottom.saturating_sub(window_rect.top);
        let client_width = client_rect.right.saturating_sub(client_rect.left);
        let client_height = client_rect.bottom.saturating_sub(client_rect.top);
        if width <= 0 || height <= 0 || client_width <= 0 || client_height <= 0 {
            return Err(PreviewCaptureError::InvalidWindowState {
                reason: "zero dimensions",
            });
        }
        let Some((capture_width, capture_height)) = extended_frame_size(hwnd) else {
            return Err(PreviewCaptureError::InvalidWindowState {
                reason: "no extended capture bounds",
            });
        };
        if capture_width != PREVIEW_WINDOW_WIDTH || capture_height != PREVIEW_WINDOW_HEIGHT {
            return Err(PreviewCaptureError::InvalidWindowState {
                reason: "preview capture bounds do not match the 640x360 contract",
            });
        }

        Ok(ValidatedWindow {
            hwnd: NativeHwnd(hwnd.0 as isize),
            width: u32::try_from(capture_width).unwrap_or(0),
            height: u32::try_from(capture_height).unwrap_or(0),
        })
    }

    fn wait_for_capture_bounds(
        hwnd: HWND,
        deadline: CaptureDeadline,
    ) -> Result<(), PreviewCaptureError> {
        const POLL_INTERVAL: Duration = Duration::from_millis(5);
        loop {
            let remaining = deadline.remaining()?;
            if extended_frame_size(hwnd) == Some((PREVIEW_WINDOW_WIDTH, PREVIEW_WINDOW_HEIGHT)) {
                return Ok(());
            }
            std::thread::sleep(remaining.min(POLL_INTERVAL));
        }
    }

    fn configure_native_window(
        hwnd: NativeHwnd,
        expected_foreground: isize,
        deadline: CaptureDeadline,
        lease: &CaptureLease,
    ) -> Result<ValidatedWindow, PreviewCaptureError> {
        lease.check(deadline)?;
        let hwnd = hwnd.as_hwnd();
        let existing_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        let requested_style = existing_style | WS_EX_NOACTIVATE.0 as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, requested_style) };
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        let mut window_rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut window_rect) }.map_err(|_| {
            PreviewCaptureError::InvalidWindowState {
                reason: "no window bounds",
            }
        })?;
        let Some((current_capture_width, current_capture_height)) = extended_frame_size(hwnd)
        else {
            return Err(PreviewCaptureError::InvalidWindowState {
                reason: "no extended capture bounds",
            });
        };
        let current_outer_width = window_rect.right.saturating_sub(window_rect.left);
        let current_outer_height = window_rect.bottom.saturating_sub(window_rect.top);
        let outer_width = current_outer_width
            .saturating_add(PREVIEW_WINDOW_WIDTH.saturating_sub(current_capture_width));
        let outer_height = current_outer_height
            .saturating_add(PREVIEW_WINDOW_HEIGHT.saturating_sub(current_capture_height));
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                outer_width,
                outer_height,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .map_err(|error| PreviewCaptureError::ApplicationFailed(error.to_string()))?;
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
            )
        }
        .map_err(|error| PreviewCaptureError::ApplicationFailed(error.to_string()))?;
        wait_for_capture_bounds(hwnd, deadline)?;

        let after = foreground_hwnd();
        if after != expected_foreground {
            return Err(PreviewCaptureError::ForegroundChanged {
                before: expected_foreground,
                after,
            });
        }
        let actual_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        if actual_style & WS_EX_NOACTIVATE.0 as isize == 0 {
            return Err(PreviewCaptureError::ApplicationFailed(
                "WS_EX_NOACTIVATE was not retained".into(),
            ));
        }
        let validated = validate_native_window(NativeHwnd(hwnd.0 as isize))?;
        lease.check(deadline)?;
        Ok(validated)
    }

    fn settle_capture(
        started: StartedCapture,
        frame: Result<CapturedFrame, PreviewCaptureError>,
        deadline: CaptureDeadline,
    ) -> Result<CapturedFrame, PreviewCaptureError> {
        let StartedCapture { control, active } = started;
        settle_capture_with_cleanup(frame, deadline, move |operation| {
            let result = control.cleanup(operation);
            drop(active);
            result
        })
    }

    fn capture_window_once(
        hwnd: NativeHwnd,
        authority: Arc<CaptureOutputAuthority>,
        expected_foreground: isize,
        deadline: CaptureDeadline,
        lease: CaptureLease,
        result_slot: Arc<Mutex<Option<Result<CaptureReport, PreviewCaptureError>>>>,
    ) -> Result<CaptureReport, PreviewCaptureError> {
        let validated = validate_native_window(hwnd)?;
        lease.check(deadline)?;
        let contract = capture_contract();
        let (sender, receiver) = mpsc::sync_channel(1);
        let settings = Settings::new(
            windows_capture::window::Window::from_raw_hwnd(hwnd.as_ptr()),
            match contract.cursor {
                CaptureSetting::Excluded => CursorCaptureSettings::WithoutCursor,
            },
            match contract.border {
                CaptureSetting::Excluded => DrawBorderSettings::WithoutBorder,
            },
            match contract.secondary_windows {
                CaptureSetting::Excluded => SecondaryWindowSettings::Exclude,
            },
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            match contract.color_format {
                CaptureColorFormat::Bgra8 => ColorFormat::Bgra8,
            },
            (sender, hwnd, lease.clone()),
        );

        let started = start_capture(settings, deadline, &lease)?;
        let frame = settle_capture(
            started,
            match receive_first_frame_with_lease(receiver, deadline, &lease) {
                Ok(Ok(frame)) => Ok(frame),
                Ok(Err(error)) => Err(error.into_capture_error()),
                Err(error) => Err(error),
            },
            deadline,
        )?;

        lease.check(deadline)?;
        if frame.width != PREVIEW_WINDOW_WIDTH as u32
            || frame.height != PREVIEW_WINDOW_HEIGHT as u32
        {
            return Err(PreviewCaptureError::InvalidWindowState {
                reason: "capture frame dimensions do not match the 640x360 contract",
            });
        }
        let after = foreground_hwnd();
        if after != expected_foreground {
            return Err(PreviewCaptureError::ForegroundChanged {
                before: expected_foreground,
                after,
            });
        }
        lease.check(deadline)?;
        let report = CaptureReport {
            width: validated.width,
            height: validated.height,
            foreground_before: expected_foreground,
            foreground_after: after,
        };
        let result_for_publication = Arc::clone(&result_slot);
        super::encode_bgra_png_atomic_with_authority_and_publication(
            authority,
            validated.width,
            validated.height,
            &frame.bgra,
            deadline,
            &lease,
            move || store_capture_result_after_commit(&result_for_publication, Ok(report)),
        )?;
        Ok(report)
    }

    pub fn capture_preview(
        root: PreviewRoot,
        request: &PreviewRequest,
    ) -> Result<CaptureReport, PreviewCaptureError> {
        let authority = Arc::clone(request.capture_authority());
        let deadline = CaptureDeadline::from_now(FIRST_FRAME_DEADLINE);
        deadline.remaining()?;
        let foreground_before = foreground_hwnd();
        let generation = CaptureGeneration::new();
        let lease = generation.begin();
        let worker_lease = lease.clone();
        let worker_authority = Arc::clone(&authority);
        let window_hold_ms = request.window_hold_ms();
        let task = spawn_capture_worker(move || {
            catch_unwind(AssertUnwindSafe(|| {
                run_preview_application(
                    root,
                    worker_authority,
                    foreground_before,
                    deadline,
                    worker_lease.clone(),
                    window_hold_ms,
                )
            }))
            .unwrap_or_else(|_| {
                Err(PreviewCaptureError::ApplicationFailed(
                    "the visible GPUI preview could not start on this desktop".into(),
                ))
            })
        })?;

        match wait_for_worker_result(task, deadline) {
            Ok(result) if lease.is_active() => Ok(result),
            Ok(result) => {
                lease.cancel();
                let _ = result;
                return Err(preserve_capture_error_after_authorized_cleanup(
                    Arc::clone(&authority),
                    PreviewCaptureError::CaptureCancelled,
                    deadline,
                ));
            }
            Err(PreviewCaptureError::DeadlineExceeded) => {
                lease.cancel();
                return Err(preserve_capture_error_after_authorized_cleanup(
                    Arc::clone(&authority),
                    PreviewCaptureError::DeadlineExceeded,
                    deadline,
                ));
            }
            Err(error) => {
                lease.cancel();
                return Err(preserve_capture_error_after_authorized_cleanup(
                    Arc::clone(&authority),
                    error.clone(),
                    deadline,
                ));
            }
        }
    }

    fn run_preview_application(
        root: PreviewRoot,
        authority: Arc<CaptureOutputAuthority>,
        foreground_before: isize,
        deadline: CaptureDeadline,
        lease: CaptureLease,
        window_hold_ms: u32,
    ) -> Result<CaptureReport, PreviewCaptureError> {
        lease.check(deadline)?;
        let result_slot = Arc::new(Mutex::new(None));
        let result_for_app = Arc::clone(&result_slot);
        let hwnd_slot = Arc::new(Mutex::new(None));
        let hwnd_for_window = Arc::clone(&hwnd_slot);
        let result_for_window = Arc::clone(&result_for_app);
        let lease_for_app = lease.clone();
        let application = gpui::Application::new().with_assets(crate::assets::AppAssets::new());
        application.run(move |cx| {
            crate::ui::preview::register_preview_environment(cx);
            let result_for_supervisor = Arc::clone(&result_for_app);
            let supervisor_lease = lease_for_app.clone();
            let supervisor_executor = cx.background_executor().clone();
            let supervision_task = cx.spawn(async move |cx| {
                supervisor_executor
                    .timer(deadline.remaining().unwrap_or_default())
                    .await;
                supervisor_lease.cancel();
                let should_quit = {
                    let mut result = result_for_supervisor
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if result.is_none() {
                        *result = Some(Err(PreviewCaptureError::DeadlineExceeded));
                        true
                    } else {
                        false
                    }
                };
                if should_quit {
                    let _ = cx.update(|cx| cx.quit());
                }
            });
            cx.set_global(CaptureDeadlineTask(supervision_task));
            let window_lease = lease_for_app.clone();
            let root_entity = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(
                            px(PREVIEW_WINDOW_WIDTH as f32),
                            px(PREVIEW_WINDOW_HEIGHT as f32),
                        ),
                        cx,
                    ))),
                    titlebar: None,
                    focus: false,
                    show: false,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Opaque,
                    ..Default::default()
                },
                move |window, cx| {
                    match hwnd_from_gpui_window(window) {
                        Ok(hwnd) => {
                            *hwnd_for_window
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hwnd)
                        }
                        Err(error) => {
                            store_capture_result(&result_for_window, &window_lease, Err(error));
                        }
                    }
                    cx.new(|_| root)
                },
            );

            let hwnd = match root_entity {
                Ok(_) => match *hwnd_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                {
                    Some(hwnd) => hwnd,
                    None => {
                        store_capture_result(
                            &result_for_app,
                            &lease_for_app,
                            Err(PreviewCaptureError::ApplicationFailed(
                                "GPUI did not expose a Windows HWND".into(),
                            )),
                        );
                        cx.quit();
                        return;
                    }
                },
                Err(error) => {
                    store_capture_result(
                        &result_for_app,
                        &lease_for_app,
                        Err(PreviewCaptureError::ApplicationFailed(error.to_string())),
                    );
                    cx.quit();
                    return;
                }
            };
            if !lease_for_app.is_active() {
                cx.quit();
                return;
            }

            if let Err(error) =
                configure_native_window(hwnd, foreground_before, deadline, &lease_for_app)
            {
                store_capture_result(&result_for_app, &lease_for_app, Err(error));
                cx.quit();
                return;
            }

            if window_hold_ms > 0 {
                std::thread::sleep(Duration::from_millis(u64::from(window_hold_ms)));
                if let Err(error) = lease_for_app.check(deadline) {
                    store_capture_result(&result_for_app, &lease_for_app, Err(error));
                    cx.quit();
                    return;
                }
            }

            let result_for_task = Arc::clone(&result_for_app);
            let capture_lease = lease_for_app.clone();
            let capture_authority = Arc::clone(&authority);
            let result_for_publication = Arc::clone(&result_for_task);
            let capture_task = cx.background_executor().spawn(async move {
                capture_window_once(
                    hwnd,
                    capture_authority,
                    foreground_before,
                    deadline,
                    capture_lease,
                    result_for_publication,
                )
            });
            let notification_lease = lease_for_app.clone();
            let notification_task = cx.spawn(async move |cx| {
                let result = capture_task.await;
                store_capture_result(&result_for_task, &notification_lease, result);
                let _ = cx.update(|cx| cx.quit());
            });
            cx.set_global(CaptureNotificationTask(notification_task));
        });

        let result = result_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| {
                if lease.is_active() {
                    Err(PreviewCaptureError::ApplicationFailed(
                        "the GPUI preview application stopped without a capture result".into(),
                    ))
                } else {
                    Err(PreviewCaptureError::CaptureCancelled)
                }
            });
        result
    }

    trait NativeHwndExt {
        fn as_hwnd(self) -> HWND;
        fn as_ptr(self) -> *mut c_void;
    }

    impl NativeHwndExt for NativeHwnd {
        fn as_hwnd(self) -> HWND {
            HWND(self.0 as *mut c_void)
        }

        fn as_ptr(self) -> *mut c_void {
            self.0 as *mut c_void
        }
    }
}

#[cfg(windows)]
pub(crate) use windows_capture_impl::capture_preview;
#[cfg(windows)]
pub use windows_capture_impl::{foreground_hwnd, validate_native_window};

#[cfg(not(windows))]
pub(crate) fn capture_preview(
    _root: PreviewRoot,
    _request: &PreviewRequest,
) -> Result<CaptureReport, PreviewCaptureError> {
    Err(PreviewCaptureError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn foreground_hwnd() -> isize {
    0
}

#[cfg(not(windows))]
pub fn validate_native_window(_hwnd: NativeHwnd) -> Result<ValidatedWindow, PreviewCaptureError> {
    Err(PreviewCaptureError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn bounded_enqueue_cleanup_returns_at_the_deadline_and_reaps_late_worker() {
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let started_at = Instant::now();

        run_bounded_cleanup(CaptureDeadline::from_now(Duration::ZERO), move || {
            started_tx.send(()).expect("cleanup worker should start");
            release_rx
                .recv()
                .map_err(|_| PreviewCaptureError::DeadlineExceeded)?;
            finished_tx.send(()).expect("cleanup worker should finish");
            Ok(())
        });

        assert!(started_at.elapsed() < Duration::from_millis(300));
        started_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("the cleanup worker must be owned after an enqueue stall");
        release_tx.send(()).expect("release late cleanup worker");
        finished_rx
            .recv_timeout(Duration::from_millis(300))
            .expect("late enqueue cleanup must eventually settle");
    }
}
