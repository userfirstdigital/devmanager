//! Visible-window native preview capture.
//!
//! Headless GPUI remains useful for structural tests, but it is not a visual
//! capture surface. The Windows path below owns the short-lived visible GPUI
//! window, captures its exact HWND, and writes only a first physical frame.

use image::ImageEncoder;
use std::cell::Cell;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
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
    output_name: OsString,
    root_identity: CaptureFileIdentity,
    parent_identity: CaptureFileIdentity,
    #[cfg(windows)]
    root_handle: Arc<std::os::windows::io::OwnedHandle>,
    #[cfg(windows)]
    parent_handle: Arc<std::os::windows::io::OwnedHandle>,
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
        #[cfg(windows)]
        {
            let (root_handle, root_identity) = open_directory_authority(&root_path)?;
            let (parent_handle, parent_identity) = open_directory_authority(&parent_path)?;
            return Ok(Self {
                root_path,
                parent_path,
                output_name,
                root_identity,
                parent_identity,
                root_handle: Arc::new(root_handle),
                parent_handle: Arc::new(parent_handle),
            });
        }
        #[cfg(not(windows))]
        {
            let root_identity = capture_file_identity(&root_path)?;
            let parent_identity = capture_file_identity(&parent_path)?;
            Ok(Self {
                root_path,
                parent_path,
                output_name,
                root_identity,
                parent_identity,
            })
        }
    }

    pub(crate) fn output_path(&self) -> PathBuf {
        self.parent_path.join(&self.output_name)
    }

    fn verify_reopened(&self) -> Result<(), PreviewCaptureError> {
        #[cfg(windows)]
        {
            let _ = self.reopen_parent_for_publication()?;
            return Ok(());
        }
        #[cfg(not(windows))]
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

    #[cfg(windows)]
    fn reopen_parent_for_publication(
        &self,
    ) -> Result<std::os::windows::io::OwnedHandle, PreviewCaptureError> {
        let (_, root_identity) = open_directory_authority(&self.root_path).map_err(|_| {
            PreviewCaptureError::OutputFailed(
                "trusted preview output root changed during capture".into(),
            )
        })?;
        let retained_root_identity =
            directory_identity_from_handle(&self.root_handle).map_err(|_| {
                PreviewCaptureError::OutputFailed(
                    "trusted preview output root handle changed during capture".into(),
                )
            })?;
        let (parent_handle, parent_identity) = open_directory_authority(&self.parent_path)
            .map_err(|_| {
                PreviewCaptureError::OutputFailed(
                    "trusted preview output parent changed during capture".into(),
                )
            })?;
        let retained_parent_identity = directory_identity_from_handle(&self.parent_handle)
            .map_err(|_| {
                PreviewCaptureError::OutputFailed(
                    "trusted preview output parent handle changed during capture".into(),
                )
            })?;
        if root_identity != self.root_identity || retained_root_identity != self.root_identity {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output root changed during capture".into(),
            ));
        }
        if parent_identity != self.parent_identity
            || retained_parent_identity != self.parent_identity
        {
            return Err(PreviewCaptureError::OutputFailed(
                "trusted preview output parent changed during capture".into(),
            ));
        }
        Ok(parent_handle)
    }
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

#[cfg(windows)]
fn open_directory_authority(
    path: &Path,
) -> Result<(std::os::windows::io::OwnedHandle, CaptureFileIdentity), PreviewCaptureError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TRAVERSE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let raw = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            (FILE_TRAVERSE | FILE_READ_ATTRIBUTES).0,
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
        atomic_publish_temp(&temp, &authority, &file).expect("handle-relative publication");
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
    let output = output.to_path_buf();
    if in_capture_executor_worker() {
        let cleanup = match fs::remove_file(output) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PreviewCaptureError::OutputFailed(error.to_string())),
        };
        return match cleanup {
            Ok(()) => primary,
            Err(error) => PreviewCaptureError::CleanupFailed(
                CleanupFailureContext::from_settlement(primary, "remove output", error),
            ),
        };
    }
    let task = match spawn_cleanup_worker(move || match fs::remove_file(output) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PreviewCaptureError::OutputFailed(error.to_string())),
    }) {
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
    let output = authority.output_path();
    let cleanup = move || {
        authority.verify_reopened()?;
        match fs::remove_file(output) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PreviewCaptureError::OutputFailed(error.to_string())),
        }
    };
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

fn capture_executor() -> &'static CaptureExecutor {
    CAPTURE_EXECUTOR.get_or_init(|| {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<CaptureJob>(CAPTURE_EXECUTOR_QUEUE);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(CAPTURE_EXECUTOR_WORKERS);
        for index in 0..CAPTURE_EXECUTOR_WORKERS {
            let receiver = Arc::clone(&receiver);
            let waiter = std::thread::Builder::new()
                .name(format!("devmanager-capture-executor-{index}"))
                .spawn(move || loop {
                    let job = receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv();
                    match job {
                        Ok(CaptureJob::Run(job)) => run_in_capture_executor_worker(job),
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
        }
    })
}

/// Deterministically stop the fixed preview executor. This is intentionally
/// explicit because the process-global executor must remain available across
/// independent preview requests; callers that own the process shutdown point
/// can use this to join every worker and prove that no capture worker remains.
#[doc(hidden)]
pub fn shutdown_capture_executor() {
    let Some(executor) = CAPTURE_EXECUTOR.get() else {
        return;
    };
    let mut workers = executor
        .workers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if workers.is_empty() {
        return;
    }
    for _ in 0..workers.len() {
        let _ = executor.sender.send(CaptureJob::Shutdown);
    }
    while let Some(worker) = workers.pop() {
        let _ = worker.join();
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
    if output.exists() {
        return Err(PreviewCaptureError::OutputAlreadyExists);
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

    let temp_path = next_temp_path(output, parent, deadline, lease)?;
    reject_reparse_ancestors(&temp_path)?;
    let mut temp = TempOutput::new(
        temp_path.clone(),
        output.to_path_buf(),
        Arc::clone(&authority),
    );
    lease.check(deadline)?;
    let mut file = open_temp_output(&temp_path).map_err(|error| {
        PreviewCaptureError::OutputFailed(format!("preview temp open failed: {error}"))
    })?;

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
    if output.exists() {
        return Err(PreviewCaptureError::OutputAlreadyExists);
    }
    lease.publish_with(deadline, || {
        atomic_publish_temp(&temp_path, &authority, &file)
            .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
        temp.renamed = true;
        drop(file);
        if let Some(callback) = publication.take() {
            callback();
        }
        Ok(())
    })?;
    temp.committed = true;
    Ok(())
}

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

/// Publish a fully synced temporary PNG without following a reparse point at
/// the final file boundary.  Windows uses the open temporary-file handle and
/// a no-follow parent handle for the rename; this avoids resolving a swapped
/// destination path after the validation pass.  Other platforms retain the
/// native atomic same-directory rename.
fn atomic_publish_temp(
    temp: &Path,
    authority: &CaptureOutputAuthority,
    file: &std::fs::File,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        return atomic_publish_temp_windows(temp, authority, file);
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        fs::rename(temp, authority.output_path())
    }
}

#[cfg(windows)]
fn atomic_publish_temp_windows(
    _temp: &Path,
    authority: &CaptureOutputAuthority,
    file: &std::fs::File,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    #[repr(C)]
    struct IoStatusBlock {
        status: i32,
        information: usize,
    }

    // The Win32 wrapper rejects a non-null RootDirectory on some supported
    // Windows builds even though the native contract accepts it.  Calling the
    // native file-information boundary keeps the destination directory
    // handle-relative and avoids falling back to a re-resolved path.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut IoStatusBlock,
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
        let mut io_status = IoStatusBlock {
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
    Ok(())
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

fn next_temp_path(
    output: &Path,
    parent: &Path,
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<PathBuf, PreviewCaptureError> {
    let stem = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("preview");
    for _ in 0..32 {
        lease.check(deadline)?;
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{stem}.{counter}.tmp"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(PreviewCaptureError::OutputFailed(
        "could not allocate a unique temporary PNG path".into(),
    ))
}

struct TempOutput {
    path: PathBuf,
    output: PathBuf,
    authority: Arc<CaptureOutputAuthority>,
    renamed: bool,
    committed: bool,
}

impl TempOutput {
    fn new(path: PathBuf, output: PathBuf, authority: Arc<CaptureOutputAuthority>) -> Self {
        Self {
            path,
            output,
            authority,
            renamed: false,
            committed: false,
        }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if self.authority.verify_reopened().is_err() {
            return;
        }
        if !self.renamed {
            let _ = fs::remove_file(&self.path);
        }
        if self.renamed && !self.committed {
            let _ = fs::remove_file(&self.output);
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

        Ok(ValidatedWindow {
            hwnd: NativeHwnd(hwnd.0 as isize),
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
        })
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
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                PREVIEW_WINDOW_WIDTH,
                PREVIEW_WINDOW_HEIGHT,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .map_err(|error| PreviewCaptureError::ApplicationFailed(error.to_string()))?;
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
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
        let _validated = validate_native_window(hwnd)?;
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
        let after = foreground_hwnd();
        if after != expected_foreground {
            return Err(PreviewCaptureError::ForegroundChanged {
                before: expected_foreground,
                after,
            });
        }
        lease.check(deadline)?;
        let report = CaptureReport {
            width: frame.width,
            height: frame.height,
            foreground_before: expected_foreground,
            foreground_after: after,
        };
        let result_for_publication = Arc::clone(&result_slot);
        super::encode_bgra_png_atomic_with_authority_and_publication(
            authority,
            frame.width,
            frame.height,
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
        let task = spawn_capture_worker(move || {
            catch_unwind(AssertUnwindSafe(|| {
                run_preview_application(
                    root,
                    worker_authority,
                    foreground_before,
                    deadline,
                    worker_lease.clone(),
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
                let _ = cleanup_authorized_output_after_deadline(
                    Arc::clone(&authority),
                    PreviewCaptureError::CaptureCancelled,
                    deadline,
                );
                Err(PreviewCaptureError::CaptureCancelled)
            }
            Err(PreviewCaptureError::DeadlineExceeded) => {
                lease.cancel();
                let _ = cleanup_authorized_output_after_deadline(
                    Arc::clone(&authority),
                    PreviewCaptureError::DeadlineExceeded,
                    deadline,
                );
                Err(PreviewCaptureError::DeadlineExceeded)
            }
            Err(error) => {
                lease.cancel();
                let _ = cleanup_authorized_output_after_deadline(
                    Arc::clone(&authority),
                    PreviewCaptureError::CaptureClosed,
                    deadline,
                );
                Err(error)
            }
        }
    }

    fn run_preview_application(
        root: PreviewRoot,
        authority: Arc<CaptureOutputAuthority>,
        foreground_before: isize,
        deadline: CaptureDeadline,
        lease: CaptureLease,
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
