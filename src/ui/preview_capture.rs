//! Visible-window native preview capture.
//!
//! Headless GPUI remains useful for structural tests, but it is not a visual
//! capture surface. The Windows path below owns the short-lived visible GPUI
//! window, captures its exact HWND, and writes only a first physical frame.

use image::ImageEncoder;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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
#[derive(Clone, Default)]
pub struct CaptureGeneration {
    next: Arc<AtomicU64>,
}

/// Coordinates the one irreversible boundary in a capture attempt: moving a
/// fully encoded temporary file into its requested output name.  Cancellation
/// is intentionally non-blocking, so a caller that has reached its deadline
/// never waits on filesystem work.  A publisher that won the boundary still
/// has to observe a cancellation request before it can mark the output as
/// committed; its temporary-output guard then removes the late file.
struct PublicationState {
    state: AtomicU8,
    cancellation_requested: AtomicBool,
}

const PUBLICATION_IDLE: u8 = 0;
const PUBLICATION_IN_FLIGHT: u8 = 1;
const PUBLICATION_COMMITTED: u8 = 2;

impl CaptureGeneration {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self) -> CaptureLease {
        let id = self.next.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        CaptureLease {
            generation: Arc::clone(&self.next),
            id,
            cancelled: Arc::new(AtomicBool::new(false)),
            publication: Arc::new(PublicationState {
                state: AtomicU8::new(PUBLICATION_IDLE),
                cancellation_requested: AtomicBool::new(false),
            }),
        }
    }
}

#[derive(Clone)]
pub struct CaptureLease {
    generation: Arc<AtomicU64>,
    id: u64,
    cancelled: Arc<AtomicBool>,
    publication: Arc<PublicationState>,
}

impl std::fmt::Debug for CaptureLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureLease")
            .field("active", &self.is_active())
            .finish()
    }
}

impl CaptureLease {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if self.publication.state.load(Ordering::Acquire) == PUBLICATION_IN_FLIGHT {
            self.publication
                .cancellation_requested
                .store(true, Ordering::Release);
        }
    }

    pub fn is_active(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == self.id
    }

    fn check(&self, deadline: CaptureDeadline) -> Result<(), PreviewCaptureError> {
        deadline.remaining()?;
        if self.is_active() {
            Ok(())
        } else {
            Err(PreviewCaptureError::CaptureCancelled)
        }
    }

    fn begin_publication(&self, deadline: CaptureDeadline) -> Result<(), PreviewCaptureError> {
        self.check(deadline)?;
        self.publication
            .state
            .compare_exchange(
                PUBLICATION_IDLE,
                PUBLICATION_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| PreviewCaptureError::CaptureCancelled)?;
        if !self.is_active() {
            self.publication
                .cancellation_requested
                .store(true, Ordering::Release);
            return Err(PreviewCaptureError::CaptureCancelled);
        }
        Ok(())
    }

    fn finish_publication(&self) -> bool {
        if self
            .publication
            .cancellation_requested
            .load(Ordering::Acquire)
            || !self.is_active()
        {
            return false;
        }
        self.publication
            .state
            .compare_exchange(
                PUBLICATION_IN_FLIGHT,
                PUBLICATION_COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
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
static CLEANUP_REAPERS: OnceLock<Mutex<Vec<CaptureCleanupReaper>>> = OnceLock::new();

pub fn active_capture_thread_count() -> usize {
    reap_cleanup_reapers();
    ACTIVE_CAPTURE_THREADS.load(Ordering::SeqCst)
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
    let (waiter, result_receiver) = spawn_cleanup_worker(move || match fs::remove_file(output) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PreviewCaptureError::OutputFailed(error.to_string())),
    });
    match wait_for_worker_result(waiter, result_receiver, deadline) {
        Ok(()) => primary,
        Err(error) => PreviewCaptureError::CleanupFailed(CleanupFailureContext::from_settlement(
            primary,
            "remove output",
            error,
        )),
    }
}

struct CaptureCleanupReaper {
    waiter: JoinHandle<()>,
}

fn cleanup_reapers() -> &'static Mutex<Vec<CaptureCleanupReaper>> {
    CLEANUP_REAPERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn retain_cleanup_reaper(waiter: JoinHandle<()>) {
    cleanup_reapers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(CaptureCleanupReaper { waiter });
}

fn reap_cleanup_reapers() {
    let mut reapers = cleanup_reapers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut pending = Vec::with_capacity(reapers.len());
    for reaper in reapers.drain(..) {
        if reaper.waiter.is_finished() {
            let _ = reaper.waiter.join();
        } else {
            pending.push(reaper);
        }
    }
    *reapers = pending;
}

fn retain_or_join_cleanup_worker(waiter: JoinHandle<()>) {
    if waiter.is_finished() {
        let _ = waiter.join();
    } else {
        retain_cleanup_reaper(waiter);
    }
}

fn spawn_cleanup_worker<F, T>(
    cleanup: F,
) -> (JoinHandle<()>, Receiver<Result<T, PreviewCaptureError>>)
where
    F: FnOnce() -> Result<T, PreviewCaptureError> + Send + 'static,
    T: Send + 'static,
{
    let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
    let waiter = std::thread::Builder::new()
        .name("devmanager-capture-cleanup".into())
        .spawn(move || {
            let result = cleanup();
            let _ = result_sender.send(result);
        })
        .expect("capture cleanup worker must be spawnable");
    (waiter, result_receiver)
}

fn wait_for_worker_result<T>(
    waiter: JoinHandle<()>,
    result_receiver: Receiver<Result<T, PreviewCaptureError>>,
    deadline: CaptureDeadline,
) -> Result<T, PreviewCaptureError>
where
    T: Send + 'static,
{
    match deadline.remaining() {
        Ok(remaining) => match result_receiver.recv_timeout(remaining) {
            Ok(result) => {
                let result = match deadline.remaining() {
                    Ok(_) => result,
                    Err(error) => Err(error),
                };
                retain_or_join_cleanup_worker(waiter);
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                retain_cleanup_reaper(waiter);
                Err(PreviewCaptureError::DeadlineExceeded)
            }
            Err(RecvTimeoutError::Disconnected) => {
                if waiter.is_finished() {
                    let _ = waiter.join();
                } else {
                    retain_cleanup_reaper(waiter);
                }
                Err(PreviewCaptureError::CaptureFailed(
                    "cleanup worker stopped without reporting a result".into(),
                ))
            }
        },
        Err(error) => {
            retain_cleanup_reaper(waiter);
            Err(error)
        }
    }
}

fn wait_for_capture_worker_result<T>(
    waiter: JoinHandle<()>,
    result_receiver: Receiver<Result<T, PreviewCaptureError>>,
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<T, PreviewCaptureError>
where
    T: Send + 'static,
{
    match wait_for_worker_result(waiter, result_receiver, deadline) {
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
    let (waiter, result_receiver) = spawn_cleanup_worker(move || {
        if !worker_lease.is_active() {
            return Err(PreviewCaptureError::CaptureCancelled);
        }
        stage(deadline, worker_lease)
    });
    wait_for_capture_worker_result(waiter, result_receiver, deadline, &lease)
}

fn run_bounded_cleanup<F>(deadline: CaptureDeadline, cleanup: F)
where
    F: FnOnce() -> Result<(), PreviewCaptureError> + Send + 'static,
{
    let (waiter, result_receiver) = spawn_cleanup_worker(cleanup);
    let _ = wait_for_worker_result(waiter, result_receiver, deadline);
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
    let (waiter, result_receiver) = spawn_cleanup_worker(move || cleanup(operation));
    let cleanup_result = wait_for_worker_result(waiter, result_receiver, deadline);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeHwnd(pub isize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedWindow {
    pub hwnd: NativeHwnd,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureReport {
    pub width: u32,
    pub height: u32,
    pub foreground_before: isize,
    pub foreground_after: isize,
}

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
    lease.check(deadline)?;
    let frame = receive_first_frame(receiver, deadline)?;
    lease.check(deadline)?;
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
    let checked_output = output
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok())
        .map(|parent| parent.join(output.file_name().unwrap_or_default()))
        .unwrap_or_else(|| output.to_path_buf());
    let checked_root = fs::canonicalize(trusted_root).map_err(|_| {
        PreviewCaptureError::OutputFailed("trusted preview output root is unavailable".into())
    })?;
    if !is_within_capture_path(&checked_output, &checked_root) {
        return Err(PreviewCaptureError::OutputFailed(
            "preview output moved outside its trusted root".into(),
        ));
    }
    encode_bgra_png_atomic(output, width, height, bgra, deadline)
}

fn is_within_capture_path(path: &Path, root: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().to_ascii_lowercase();
        let root = root.to_string_lossy().to_ascii_lowercase();
        let root = root.trim_end_matches(['\\', '/']);
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with(['\\', '/']))
    }
    #[cfg(not(windows))]
    {
        false
    }
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
    )
}

fn encode_bgra_png_atomic_owned(
    output: PathBuf,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    deadline: CaptureDeadline,
    lease: CaptureLease,
) -> Result<(), PreviewCaptureError> {
    lease.check(deadline)?;
    let lease_for_worker = lease.clone();
    let (waiter, result_receiver) = spawn_cleanup_worker(move || {
        encode_bgra_png_atomic_sync(&output, width, height, &bgra, deadline, &lease_for_worker)
    });
    wait_for_capture_worker_result(waiter, result_receiver, deadline, &lease)
}

fn encode_bgra_png_atomic_sync(
    output: &Path,
    width: u32,
    height: u32,
    bgra: &[u8],
    deadline: CaptureDeadline,
    lease: &CaptureLease,
) -> Result<(), PreviewCaptureError> {
    lease.check(deadline)?;
    reject_reparse_ancestors(output)?;
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
    reject_reparse_ancestors(parent)?;
    fs::create_dir_all(parent)
        .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
    lease.check(deadline)?;

    let temp_path = next_temp_path(output, parent, deadline, lease)?;
    reject_reparse_ancestors(&temp_path)?;
    let mut temp = TempOutput::new(temp_path.clone(), output.to_path_buf());
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
    lease.check(deadline)?;
    if output.exists() {
        return Err(PreviewCaptureError::OutputAlreadyExists);
    }
    lease.begin_publication(deadline)?;
    atomic_publish_temp(&temp_path, output, &file)
        .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
    temp.renamed = true;
    drop(file);
    if deadline.remaining().is_err() {
        lease.cancel();
        return Err(PreviewCaptureError::DeadlineExceeded);
    }
    if !lease.finish_publication() {
        return Err(PreviewCaptureError::CaptureCancelled);
    }
    temp.committed = true;
    Ok(())
}

fn open_temp_output(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.access_mode(
            windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0
                | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0
                | windows::Win32::Storage::FileSystem::DELETE.0,
        );
        options.custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    options.open(path)
}

/// Publish a fully synced temporary PNG without following a reparse point at
/// the final file boundary.  Windows uses the open temporary-file handle and
/// a no-follow parent handle for the rename; this avoids resolving a swapped
/// destination path after the validation pass.  Other platforms retain the
/// native atomic same-directory rename.
fn atomic_publish_temp(temp: &Path, output: &Path, file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        return atomic_publish_temp_windows(temp, output, file);
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        fs::rename(temp, output)
    }
}

#[cfg(windows)]
fn atomic_publish_temp_windows(
    temp: &Path,
    output: &Path,
    file: &std::fs::File,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FileRenameInfo, GetFileInformationByHandle, SetFileInformationByHandle,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let parent = output.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "PNG output has no parent directory",
        )
    })?;
    let file_name = output.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "PNG output has no file name",
        )
    })?;
    let file_name: Vec<u16> = file_name.encode_wide().collect();
    let parent_wide: Vec<u16> = parent
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let parent_handle = unsafe {
        windows::Win32::Storage::FileSystem::CreateFileW(
            windows::core::PCWSTR(parent_wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|error| windows_io_error("preview parent open", error))?;
    let parent_handle =
        unsafe { OwnedHandle::from_raw_handle(parent_handle.0 as *mut std::ffi::c_void) };

    let mut parent_information =
        windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(
            HANDLE(parent_handle.as_raw_handle()),
            &mut parent_information,
        )
    }
    .map_err(|error| windows_io_error("preview parent identity", error))?;
    if parent_information.dwFileAttributes
        & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
        != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "PNG output parent is a reparse point",
        ));
    }

    let file_handle = HANDLE(file.as_raw_handle());

    let bytes = file_name.len().checked_mul(2).ok_or_else(|| {
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
    let info = rename.as_mut_ptr() as *mut windows::Win32::Storage::FileSystem::FILE_RENAME_INFO;
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
        let result = SetFileInformationByHandle(
            file_handle,
            FileRenameInfo,
            info.cast(),
            u32::try_from(total).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long")
            })?,
        );
        if let Err(error) = result {
            // A small set of older Windows filesystems reject a relative
            // FILE_RENAME_INFO even when the handles are valid.  Preserve the
            // no-overwrite contract with MoveFileExW (without
            // MOVEFILE_REPLACE_EXISTING) rather than falling back to a
            // replacing std::fs::rename.  The parent was already opened
            // with OPEN_REPARSE_POINT and checked above.
            if error.code().0 == -2_147_024_809 {
                return atomic_publish_temp_windows_fallback(temp, output);
            }
            return Err(windows_io_error("preview relative rename", error));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_publish_temp_windows_fallback(temp: &Path, output: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let temp_wide: Vec<u16> = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let output_wide: Vec<u16> = output
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MoveFileExW(
            windows::core::PCWSTR(temp_wide.as_ptr()),
            windows::core::PCWSTR(output_wide.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| windows_io_error("preview fallback rename", error))
}

#[cfg(windows)]
fn windows_io_error(stage: &str, error: windows::core::Error) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("{stage} failed ({})", error.code().0),
    )
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
    renamed: bool,
    committed: bool,
}

impl TempOutput {
    fn new(path: PathBuf, output: PathBuf) -> Self {
        Self {
            path,
            output,
            renamed: false,
            committed: false,
        }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
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

    fn cleanup_started_capture_after_enqueue_failure(started: StartedCapture) {
        let StartedCapture { control, active } = started;
        let deadline = control.deadline;
        run_bounded_cleanup(deadline, move || {
            let result = control.cleanup(CaptureCleanupOperation::Stop);
            drop(active);
            result
        });
    }

    type CaptureSettings = Settings<
        (
            SyncSender<Result<CapturedFrame, HandlerError>>,
            NativeHwnd,
            CaptureLease,
        ),
        windows_capture::window::Window,
    >;

    fn start_capture(
        settings: CaptureSettings,
        deadline: CaptureDeadline,
        lease: &CaptureLease,
    ) -> Result<StartedCapture, PreviewCaptureError> {
        lease.check(deadline)?;
        let lease_for_worker = lease.clone();
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let waiter = std::thread::Builder::new()
            .name("devmanager-capture-start".into())
            .spawn(move || {
                if !lease_for_worker.is_active() {
                    let _ = result_sender.send(Err(PreviewCaptureError::CaptureCancelled));
                    return;
                }
                let active = ActiveCaptureGuard::new();
                let result = FirstFrameHandler::start_free_threaded(settings)
                    .map_err(map_graphics_capture_error);
                match result {
                    Ok(control) => {
                        if !lease_for_worker.is_active() {
                            drop(CaptureControlGuard::new(control, deadline));
                            drop(active);
                            let _ = result_sender.send(Err(PreviewCaptureError::CaptureCancelled));
                            return;
                        }
                        let started = StartedCapture {
                            control: CaptureControlGuard::new(control, deadline),
                            active,
                        };
                        if let Err(error) = result_sender.send(Ok(started)) {
                            if let Ok(started) = error.0 {
                                cleanup_started_capture_after_enqueue_failure(started);
                            }
                        }
                    }
                    Err(error) => {
                        drop(active);
                        let _ = result_sender.send(Err(error));
                    }
                }
            })
            .expect("capture startup worker must be spawnable");

        match deadline.remaining() {
            Ok(remaining) => match result_receiver.recv_timeout(remaining) {
                Ok(result) => {
                    retain_or_join_cleanup_worker(waiter);
                    if let Err(error) = deadline.remaining() {
                        if let Ok(started) = result {
                            drop(started);
                        }
                        Err(error)
                    } else {
                        result
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    lease.cancel();
                    retain_cleanup_reaper(waiter);
                    Err(PreviewCaptureError::DeadlineExceeded)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    retain_or_join_cleanup_worker(waiter);
                    Err(PreviewCaptureError::CaptureFailed(
                        "capture startup worker stopped without reporting a result".into(),
                    ))
                }
            },
            Err(error) => {
                lease.cancel();
                retain_cleanup_reaper(waiter);
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
        output: PathBuf,
        expected_foreground: isize,
        deadline: CaptureDeadline,
        lease: CaptureLease,
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
        encode_bgra_png_atomic_owned(
            output.clone(),
            frame.width,
            frame.height,
            frame.bgra,
            deadline,
            lease.clone(),
        )?;
        lease.check(deadline)?;
        Ok(CaptureReport {
            width: frame.width,
            height: frame.height,
            foreground_before: expected_foreground,
            foreground_after: after,
        })
    }

    pub fn capture_preview(
        root: PreviewRoot,
        request: &PreviewRequest,
    ) -> Result<CaptureReport, PreviewCaptureError> {
        let output = request.output_path().to_path_buf();
        let deadline = CaptureDeadline::from_now(FIRST_FRAME_DEADLINE);
        deadline.remaining()?;
        let foreground_before = foreground_hwnd();
        let generation = CaptureGeneration::new();
        let lease = generation.begin();
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let worker_lease = lease.clone();
        let worker = std::thread::Builder::new()
            .name("devmanager-preview-application".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_preview_application(
                        root,
                        output,
                        foreground_before,
                        deadline,
                        worker_lease.clone(),
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(PreviewCaptureError::ApplicationFailed(
                        "the visible GPUI preview could not start on this desktop".into(),
                    ))
                });
                let _ = result_sender.send(result);
            })
            .map_err(|error| PreviewCaptureError::ApplicationFailed(error.to_string()))?;

        match result_receiver.recv_timeout(deadline.remaining()?) {
            Ok(result) if lease.is_active() => {
                retain_or_join_cleanup_worker(worker);
                settle_capture_result(request.output_path(), result, deadline)
            }
            Ok(result) => {
                lease.cancel();
                retain_or_join_cleanup_worker(worker);
                let _ = result;
                let _ = cleanup_output_after_deadline(
                    request.output_path(),
                    PreviewCaptureError::CaptureCancelled,
                    deadline,
                );
                Err(PreviewCaptureError::CaptureCancelled)
            }
            Err(RecvTimeoutError::Timeout) => {
                lease.cancel();
                retain_cleanup_reaper(worker);
                let _ = cleanup_output_after_deadline(
                    request.output_path(),
                    PreviewCaptureError::DeadlineExceeded,
                    deadline,
                );
                Err(PreviewCaptureError::DeadlineExceeded)
            }
            Err(RecvTimeoutError::Disconnected) => {
                lease.cancel();
                retain_or_join_cleanup_worker(worker);
                let _ = cleanup_output_after_deadline(
                    request.output_path(),
                    PreviewCaptureError::CaptureClosed,
                    deadline,
                );
                Err(PreviewCaptureError::CaptureFailed(
                    "preview application worker stopped without reporting a result".into(),
                ))
            }
        }
    }

    fn run_preview_application(
        root: PreviewRoot,
        output: PathBuf,
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
            let capture_task = cx.background_executor().spawn(async move {
                capture_window_once(hwnd, output, foreground_before, deadline, capture_lease)
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
