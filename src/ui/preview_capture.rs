//! Visible-window native preview capture.
//!
//! Headless GPUI remains useful for structural tests, but it is not a visual
//! capture surface. The Windows path below owns the short-lived visible GPUI
//! window, captures its exact HWND, and writes only a first physical frame.

use image::ImageEncoder;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
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

fn spawn_cleanup_worker<F>(
    cleanup: F,
) -> (JoinHandle<()>, Receiver<Result<(), PreviewCaptureError>>)
where
    F: FnOnce() -> Result<(), PreviewCaptureError> + Send + 'static,
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
    let cleanup_result = match deadline.remaining() {
        Ok(remaining) => match result_receiver.recv_timeout(remaining) {
            Ok(result) => {
                retain_or_join_cleanup_worker(waiter);
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                retain_cleanup_reaper(waiter);
                Err(PreviewCaptureError::CaptureFailed(
                    "cleanup deadline exceeded before the operation completed".into(),
                ))
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
        Err(_) => {
            retain_cleanup_reaper(waiter);
            Err(PreviewCaptureError::CaptureFailed(
                "cleanup deadline exceeded before cleanup could settle".into(),
            ))
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewCaptureError {
    UnsupportedPlatform,
    InvalidHwnd,
    ForeignHwnd,
    InvalidWindowState { reason: &'static str },
    DeadlineExceeded,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        if value.len() > payload_available {
            let boundary = utf8_prefix_len(value, payload_available);
            self.rendered.push_str(&value[..boundary]);
            self.truncate();
        } else {
            self.rendered.push_str(value);
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
            let _ = write!(
                diagnostic,
                "foreground HWND changed during capture (before {before:#x}, after {after:#x})"
            );
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

impl std::error::Error for PreviewCaptureError {}

pub fn receive_first_frame<T>(
    receiver: Receiver<T>,
    deadline: CaptureDeadline,
) -> Result<T, PreviewCaptureError> {
    match receiver.recv_timeout(deadline.remaining()?) {
        Ok(frame) => Ok(frame),
        Err(RecvTimeoutError::Timeout) => Err(PreviewCaptureError::DeadlineExceeded),
        Err(RecvTimeoutError::Disconnected) => Err(PreviewCaptureError::CaptureClosed),
    }
}

pub(crate) fn encode_bgra_png_atomic(
    output: &Path,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<(), PreviewCaptureError> {
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
    fs::create_dir_all(parent)
        .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;

    let temp_path = next_temp_path(output, parent)?;
    let mut temp = TempOutput::new(temp_path.clone());
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;

    let mut rgba = Vec::with_capacity(expected_bytes);
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }

    {
        let encoder = image::codecs::png::PngEncoder::new(file);
        encoder
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|error| PreviewCaptureError::PngFailed(error.to_string()))?;
    }

    if output.exists() {
        return Err(PreviewCaptureError::OutputAlreadyExists);
    }
    fs::rename(&temp_path, output)
        .map_err(|error| PreviewCaptureError::OutputFailed(error.to_string()))?;
    temp.committed = true;
    Ok(())
}

fn next_temp_path(output: &Path, parent: &Path) -> Result<PathBuf, PreviewCaptureError> {
    let stem = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("preview");
    for _ in 0..32 {
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
    committed: bool,
}

impl TempOutput {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
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
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::rc::Rc;
    use std::sync::mpsc::{self, Sender};
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
        sender: Sender<Result<CapturedFrame, HandlerError>>,
        hwnd: NativeHwnd,
    }

    impl FirstFrameHandler {
        fn send_error(&self, error: PreviewCaptureError, capture_control: InternalCaptureControl) {
            let _ = self.sender.send(Err(HandlerError(error)));
            capture_control.stop();
        }
    }

    #[allow(dead_code)]
    struct CaptureNotificationTask(gpui::Task<()>);

    impl gpui::Global for CaptureNotificationTask {}

    impl GraphicsCaptureApiHandler for FirstFrameHandler {
        type Flags = (Sender<Result<CapturedFrame, HandlerError>>, NativeHwnd);
        type Error = HandlerError;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self {
                sender: ctx.flags.0,
                hwnd: ctx.flags.1,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
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

            let _ = self.sender.send(Ok(CapturedFrame {
                width,
                height,
                bgra,
            }));
            capture_control.stop();
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            let _ = self
                .sender
                .send(Err(HandlerError(PreviewCaptureError::CaptureClosed)));
            Ok(())
        }
    }

    struct CaptureControlGuard {
        control: Option<CaptureControl<FirstFrameHandler, HandlerError>>,
    }

    impl CaptureControlGuard {
        fn new(control: CaptureControl<FirstFrameHandler, HandlerError>) -> Self {
            Self {
                control: Some(control),
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
            let (waiter, _result_receiver) = spawn_cleanup_worker(move || {
                control.stop().map_err(|error| {
                    PreviewCaptureError::CaptureFailed(format!(
                        "capture guard drop cleanup: {error}"
                    ))
                })
            });
            retain_cleanup_reaper(waiter);
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
        (Sender<Result<CapturedFrame, HandlerError>>, NativeHwnd),
        windows_capture::window::Window,
    >;

    fn start_capture(
        settings: CaptureSettings,
        deadline: CaptureDeadline,
    ) -> Result<StartedCapture, PreviewCaptureError> {
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let waiter = std::thread::Builder::new()
            .name("devmanager-capture-start".into())
            .spawn(move || {
                let active = ActiveCaptureGuard::new();
                let result = FirstFrameHandler::start_free_threaded(settings)
                    .map_err(map_graphics_capture_error);
                match result {
                    Ok(control) => {
                        let started = StartedCapture {
                            control: CaptureControlGuard::new(control),
                            active,
                        };
                        if let Err(error) = result_sender.send(Ok(started)) {
                            if let Ok(started) = error.0 {
                                let _ = started.control.cleanup(CaptureCleanupOperation::Stop);
                                drop(started.active);
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
                    result
                }
                Err(RecvTimeoutError::Timeout) => {
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
    ) -> Result<ValidatedWindow, PreviewCaptureError> {
        deadline.remaining()?;
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
        deadline.remaining()?;
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
    ) -> Result<CaptureReport, PreviewCaptureError> {
        let _validated = validate_native_window(hwnd)?;
        deadline.remaining()?;
        let contract = capture_contract();
        let (sender, receiver) = mpsc::channel();
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
            (sender, hwnd),
        );

        let started = start_capture(settings, deadline)?;
        let frame = settle_capture(
            started,
            match receive_first_frame(receiver, deadline) {
                Ok(Ok(frame)) => Ok(frame),
                Ok(Err(error)) => Err(error.into_capture_error()),
                Err(error) => Err(error),
            },
            deadline,
        )?;

        deadline.remaining()?;
        let after = foreground_hwnd();
        if after != expected_foreground {
            return Err(PreviewCaptureError::ForegroundChanged {
                before: expected_foreground,
                after,
            });
        }
        deadline.remaining()?;
        encode_bgra_png_atomic(&output, frame.width, frame.height, &frame.bgra)?;
        deadline.remaining()?;
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
        let result_slot = Arc::new(std::sync::Mutex::new(None));
        let result_for_app = Arc::clone(&result_slot);
        let hwnd_slot = Rc::new(RefCell::new(None));
        let hwnd_for_window = Rc::clone(&hwnd_slot);
        let result_for_window = Arc::clone(&result_for_app);
        let application = gpui::Application::new().with_assets(crate::assets::AppAssets::new());

        let run_result = catch_unwind(AssertUnwindSafe(|| {
            application.run(move |cx| {
                crate::ui::preview::register_preview_environment(cx);
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
                            Ok(hwnd) => *hwnd_for_window.borrow_mut() = Some(hwnd),
                            Err(error) => {
                                let _ = result_for_window.lock().unwrap().replace(Err(error));
                            }
                        }
                        cx.new(|_| root)
                    },
                );

                let hwnd = match root_entity {
                    Ok(_) => match *hwnd_slot.borrow() {
                        Some(hwnd) => hwnd,
                        None => {
                            let _ = result_for_app.lock().unwrap().replace(Err(
                                PreviewCaptureError::ApplicationFailed(
                                    "GPUI did not expose a Windows HWND".into(),
                                ),
                            ));
                            cx.quit();
                            return;
                        }
                    },
                    Err(error) => {
                        let _ = result_for_app.lock().unwrap().replace(Err(
                            PreviewCaptureError::ApplicationFailed(error.to_string()),
                        ));
                        cx.quit();
                        return;
                    }
                };
                if result_for_app.lock().unwrap().is_some() {
                    cx.quit();
                    return;
                }

                if let Err(error) = configure_native_window(hwnd, foreground_before, deadline) {
                    let _ = result_for_app.lock().unwrap().replace(Err(error));
                    cx.quit();
                    return;
                }

                let result_for_task = Arc::clone(&result_for_app);
                let capture_task = cx.background_executor().spawn(async move {
                    capture_window_once(hwnd, output, foreground_before, deadline)
                });
                let notification_task = cx.spawn(async move |cx| {
                    let result = capture_task.await;
                    let _ = result_for_task.lock().unwrap().replace(result);
                    let _ = cx.update(|cx| cx.quit());
                });
                cx.set_global(CaptureNotificationTask(notification_task));
            });
        }));

        if run_result.is_err() {
            return Err(PreviewCaptureError::ApplicationFailed(
                "the visible GPUI preview could not start on this desktop".into(),
            ));
        }
        let result = match result_slot.lock().unwrap().take() {
            Some(result) => result,
            None => match deadline.remaining() {
                Ok(_) => Err(PreviewCaptureError::ApplicationFailed(
                    "the GPUI preview application stopped without a capture result".into(),
                )),
                Err(error) => Err(error),
            },
        };
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
