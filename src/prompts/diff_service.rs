//! Host-side exact-version orchestration for bounded prompt diffs.
//!
//! This module deliberately owns no persistence adapter and has no UI/runtime
//! dependencies. A later store adapter supplies [`ExactPromptVersionLoader`],
//! while a host worker calls its actor-confined exact computation away from
//! paint and input handling. The cache contains only the body-free public projection;
//! any text view is transient in the worker response and is never serialized or
//! retained by the service cache.

use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::domain::PromptVersionId;

use super::diff::{
    diff_versions_with_budget, encode_public_diff, DiffBudget, DiffStatus, InlineSpan,
    InlineSpanKind, LineChangeKind, LineHunk, LineHunkKind, PromptDiff, PromptDiffEncodeError,
    TruncationMarker, UnicodeRange, MAX_PROMPT_DIFF_PAYLOAD_BYTES, PROMPT_DIFF_ALGORITHM_VERSION,
    PROMPT_DIFF_NORMALIZATION_POLICY, PROMPT_DIFF_PUBLIC_PROJECTION_VERSION,
};
use super::model::MAX_PROMPT_BODY_BYTES;

/// Metadata returned before any prompt body bytes are allocated or copied.
///
/// A host/store adapter must provide this exact record first. The worker rejects
/// a body larger than [`MAX_PROMPT_BODY_BYTES`] before it creates a body writer,
/// so a corrupt multi-megabyte row cannot become a service-owned allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExactPromptVersionMetadata {
    id: PromptVersionId,
    body_bytes: usize,
    body_sha256: [u8; 32],
}

impl ExactPromptVersionMetadata {
    pub fn new(id: PromptVersionId, body_bytes: usize, body_sha256: [u8; 32]) -> Self {
        Self {
            id,
            body_bytes,
            body_sha256,
        }
    }

    pub fn id(&self) -> PromptVersionId {
        self.id
    }

    pub fn body_bytes(&self) -> usize {
        self.body_bytes
    }

    pub fn body_sha256(&self) -> [u8; 32] {
        self.body_sha256
    }
}

impl fmt::Debug for ExactPromptVersionMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExactPromptVersionMetadata")
            .field("id", &self.id)
            .field("body_bytes", &self.body_bytes)
            .field("body_sha256", &self.body_sha256)
            .finish()
    }
}

/// Bounded destination supplied to an exact loader's streaming read.
///
/// The adapter can append chunks but cannot extract, serialize, or retain the
/// body through this public transport boundary. The worker creates this writer
/// only after validating metadata length against the 256 KiB body cap.
pub struct PromptVersionBodyWriter {
    expected_bytes: usize,
    bytes: Vec<u8>,
}

impl PromptVersionBodyWriter {
    fn new(expected_bytes: usize) -> Self {
        Self {
            expected_bytes,
            bytes: Vec::with_capacity(expected_bytes),
        }
    }

    pub fn bytes_written(&self) -> usize {
        self.bytes.len()
    }

    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), PromptDiffServiceError> {
        let Some(next) = self.bytes.len().checked_add(chunk.len()) else {
            return Err(PromptDiffServiceError::BodyBytesExceeded {
                expected_bytes: self.expected_bytes,
                attempted_bytes: usize::MAX,
            });
        };
        if next > self.expected_bytes || next > MAX_PROMPT_BODY_BYTES {
            return Err(PromptDiffServiceError::BodyBytesExceeded {
                expected_bytes: self.expected_bytes,
                attempted_bytes: next,
            });
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Exact-version loading authority injected by the host/store integration.
///
/// This is the sealed worker seam for the f06e6ee prompt-store adapter: first
/// return metadata, then stream the exact immutable body into the worker-owned
/// bounded writer. There is deliberately no `latest`, `current`, cwd,
/// timestamp, or transcript fallback in this trait.
pub trait ExactPromptVersionLoader: Send + 'static {
    fn load_exact_metadata(
        &mut self,
        id: PromptVersionId,
    ) -> Result<ExactPromptVersionMetadata, PromptDiffServiceError>;

    fn read_exact_body(
        &mut self,
        id: PromptVersionId,
        writer: &mut PromptVersionBodyWriter,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<(), PromptDiffServiceError>;
}

/// A pair of exact immutable IDs and their expected body fingerprints.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExactPromptDiffRequest {
    before_id: PromptVersionId,
    after_id: PromptVersionId,
    before_body_sha256: [u8; 32],
    after_body_sha256: [u8; 32],
    generation: u64,
}

impl ExactPromptDiffRequest {
    pub fn new(
        before_id: PromptVersionId,
        after_id: PromptVersionId,
        before_body_sha256: [u8; 32],
        after_body_sha256: [u8; 32],
        generation: u64,
    ) -> Self {
        Self {
            before_id,
            after_id,
            before_body_sha256,
            after_body_sha256,
            generation,
        }
    }

    pub fn before_id(&self) -> PromptVersionId {
        self.before_id
    }

    pub fn after_id(&self) -> PromptVersionId {
        self.after_id
    }

    pub fn before_body_sha256(&self) -> &[u8; 32] {
        &self.before_body_sha256
    }

    pub fn after_body_sha256(&self) -> &[u8; 32] {
        &self.after_body_sha256
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn with_generation(self, generation: u64) -> Self {
        Self { generation, ..self }
    }
}

impl fmt::Debug for ExactPromptDiffRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExactPromptDiffRequest")
            .field("before_id", &self.before_id)
            .field("after_id", &self.after_id)
            .field("before_body_sha256", &self.before_body_sha256)
            .field("after_body_sha256", &self.after_body_sha256)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Explicit failures from exact-version resolution or delivery fencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDiffServiceError {
    MissingVersion {
        id: PromptVersionId,
    },
    CorruptVersion {
        id: PromptVersionId,
    },
    OversizedVersion {
        id: PromptVersionId,
        body_bytes: usize,
        max_bytes: usize,
    },
    BodyBytesExceeded {
        expected_bytes: usize,
        attempted_bytes: usize,
    },
    StaleVersion {
        id: PromptVersionId,
    },
    Cancelled,
    DeadlineExceeded,
    StaleGeneration {
        requested: u64,
        current: u64,
    },
    ProjectionLimit {
        encoded_bytes: usize,
        max_bytes: usize,
    },
}

impl fmt::Display for PromptDiffServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVersion { id } => write!(f, "prompt version {id} is missing"),
            Self::CorruptVersion { id } => write!(f, "prompt version {id} is corrupt"),
            Self::OversizedVersion {
                id,
                body_bytes,
                max_bytes,
            } => write!(
                f,
                "prompt version {id} is {body_bytes} bytes; maximum is {max_bytes}"
            ),
            Self::BodyBytesExceeded {
                expected_bytes,
                attempted_bytes,
            } => write!(
                f,
                "prompt body stream wrote {attempted_bytes} bytes; expected {expected_bytes}"
            ),
            Self::StaleVersion { id } => write!(f, "prompt version {id} is stale"),
            Self::Cancelled => f.write_str("prompt diff was cancelled"),
            Self::DeadlineExceeded => f.write_str("prompt diff deadline expired"),
            Self::StaleGeneration { requested, current } => write!(
                f,
                "prompt diff generation {requested} is stale; current generation is {current}"
            ),
            Self::ProjectionLimit {
                encoded_bytes,
                max_bytes,
            } => write!(
                f,
                "prompt diff projection is {encoded_bytes} bytes; maximum is {max_bytes}"
            ),
        }
    }
}

impl std::error::Error for PromptDiffServiceError {}

impl From<PromptDiffEncodeError> for PromptDiffServiceError {
    fn from(error: PromptDiffEncodeError) -> Self {
        match error {
            PromptDiffEncodeError::PayloadLimit {
                encoded_bytes,
                max_bytes,
            } => Self::ProjectionLimit {
                encoded_bytes,
                max_bytes,
            },
        }
    }
}

/// Body-free response delivered from the host worker.
pub struct PromptDiffServiceResponse {
    request: ExactPromptDiffRequest,
    status: DiffStatus,
    public_projection: Vec<u8>,
    #[allow(dead_code)]
    local_projection: Option<LocalPromptDiffProjection>,
    cache_hit: bool,
}

impl PromptDiffServiceResponse {
    pub fn before_id(&self) -> PromptVersionId {
        self.request.before_id
    }

    pub fn after_id(&self) -> PromptVersionId {
        self.request.after_id
    }

    pub fn before_body_sha256(&self) -> &[u8; 32] {
        &self.request.before_body_sha256
    }

    pub fn after_body_sha256(&self) -> &[u8; 32] {
        &self.request.after_body_sha256
    }

    pub fn generation(&self) -> u64 {
        self.request.generation
    }

    pub fn status(&self) -> DiffStatus {
        self.status
    }

    /// Metadata-only JSON: hashes, ranges, lengths, status, and truncation.
    /// Raw line/span text is intentionally unavailable through this transport.
    pub fn public_projection(&self) -> &[u8] {
        &self.public_projection
    }

    pub fn cache_hit(&self) -> bool {
        self.cache_hit
    }

    /// Whether this worker response carries a fresh crate-local text view.
    /// The view itself is never part of the public transport methods.
    pub fn has_local_projection(&self) -> bool {
        self.local_projection.is_some()
    }

    /// The bounded, body-carrying projection intended only for crate-local UI
    /// rendering. It is absent on a body-free cache hit; the host may request a
    /// fresh worker result when local text is needed.
    #[allow(dead_code)]
    pub(crate) fn local_projection(&self) -> Option<&LocalPromptDiffProjection> {
        self.local_projection.as_ref()
    }
}

impl fmt::Debug for PromptDiffServiceResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptDiffServiceResponse")
            .field("before_id", &self.before_id())
            .field("after_id", &self.after_id())
            .field("before_body_sha256", &self.before_body_sha256())
            .field("after_body_sha256", &self.after_body_sha256())
            .field("generation", &self.generation())
            .field("status", &self.status)
            .field("public_projection_bytes", &self.public_projection.len())
            .field("cache_hit", &self.cache_hit)
            .finish()
    }
}

/// Exactly one request may wait behind the request currently being executed.
/// A newer accepted request cancels and fences the active request.
pub const PROMPT_DIFF_WORKER_QUEUE_CAPACITY: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDiffWorkerError {
    QueueFull { generation: u64 },
    Closed,
    JoinFailed,
}

impl fmt::Display for PromptDiffWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull { generation } => {
                write!(
                    f,
                    "prompt diff worker queue is full for generation {generation}"
                )
            }
            Self::Closed => f.write_str("prompt diff worker is closed"),
            Self::JoinFailed => f.write_str("prompt diff worker did not join cleanly"),
        }
    }
}

impl std::error::Error for PromptDiffWorkerError {}

/// A submitted request's cancellation handle. The handle is independent of
/// the worker's generation fence, so callers can cancel one request without
/// mutating another request's identity or result metadata.
pub struct PromptDiffWorkerSubmission {
    request: ExactPromptDiffRequest,
    cancellation: Arc<AtomicBool>,
}

impl PromptDiffWorkerSubmission {
    pub fn request(&self) -> ExactPromptDiffRequest {
        self.request
    }

    pub fn generation(&self) -> u64 {
        self.request.generation
    }

    pub fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }
}

impl fmt::Debug for PromptDiffWorkerSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptDiffWorkerSubmission")
            .field("request", &self.request)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// A body-free result envelope. The response's crate-local projection is
/// still owned by the worker result and is never placed in the worker cache or
/// exposed through this envelope's debug/serialization surface.
pub struct PromptDiffWorkerResult {
    request: ExactPromptDiffRequest,
    outcome: Result<PromptDiffServiceResponse, PromptDiffServiceError>,
}

impl PromptDiffWorkerResult {
    pub fn request(&self) -> ExactPromptDiffRequest {
        self.request
    }

    pub fn before_id(&self) -> PromptVersionId {
        self.request.before_id
    }

    pub fn after_id(&self) -> PromptVersionId {
        self.request.after_id
    }

    pub fn before_body_sha256(&self) -> &[u8; 32] {
        &self.request.before_body_sha256
    }

    pub fn after_body_sha256(&self) -> &[u8; 32] {
        &self.request.after_body_sha256
    }

    pub fn generation(&self) -> u64 {
        self.request.generation
    }

    pub fn outcome(self) -> Result<PromptDiffServiceResponse, PromptDiffServiceError> {
        self.outcome
    }
}

impl fmt::Debug for PromptDiffWorkerResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome = match &self.outcome {
            Ok(response) => format!("Ok(status={:?})", response.status()),
            Err(error) => format!("Err({error:?})"),
        };
        f.debug_struct("PromptDiffWorkerResult")
            .field("request", &self.request)
            .field("outcome", &outcome)
            .finish()
    }
}

struct WorkerRequest {
    request: ExactPromptDiffRequest,
    cancellation: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

struct WorkerShared {
    shutdown: AtomicBool,
    next_generation: AtomicU64,
    latest_generation: AtomicU64,
    fence: Mutex<()>,
    active_cancellation: Mutex<Option<ActiveCancellation>>,
}

struct ActiveCancellation {
    generation: u64,
    token: Arc<AtomicBool>,
}

impl WorkerShared {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            next_generation: AtomicU64::new(0),
            latest_generation: AtomicU64::new(0),
            fence: Mutex::new(()),
            active_cancellation: Mutex::new(None),
        }
    }

    fn cancel_active_except(&self, generation: u64) {
        if let Ok(active) = self.active_cancellation.lock() {
            if let Some(active) = active.as_ref() {
                if active.generation != generation {
                    active.token.store(true, Ordering::Release);
                }
            }
        }
    }

    fn cancel_active(&self) {
        if let Ok(active) = self.active_cancellation.lock() {
            if let Some(active) = active.as_ref() {
                active.token.store(true, Ordering::Release);
            }
        }
    }

    fn set_active(&self, generation: u64, cancellation: Arc<AtomicBool>) {
        *self
            .active_cancellation
            .lock()
            .expect("prompt diff active cancellation lock poisoned") = Some(ActiveCancellation {
            generation,
            token: cancellation,
        });
    }

    fn clear_active(&self) {
        *self
            .active_cancellation
            .lock()
            .expect("prompt diff active cancellation lock poisoned") = None;
    }
}

/// Owned host worker for exact prompt diffs.
///
/// The public submission/result surface is asynchronous. The synchronous
/// service and all prompt body storage live exclusively on the actor thread;
/// callers from paint/input code can only enqueue, cancel, and poll.
pub struct PromptDiffWorker<L> {
    sender: Option<SyncSender<WorkerRequest>>,
    results: Mutex<Receiver<PromptDiffWorkerResult>>,
    shared: Arc<WorkerShared>,
    join: Option<JoinHandle<()>>,
    _loader: PhantomData<L>,
}

impl<L: ExactPromptVersionLoader> PromptDiffWorker<L> {
    pub fn spawn(loader: L, max_items: usize, max_bytes: usize) -> Self {
        let (sender, requests) = mpsc::sync_channel(PROMPT_DIFF_WORKER_QUEUE_CAPACITY);
        let (results_sender, results) = mpsc::channel();
        let shared = Arc::new(WorkerShared::new());
        let worker_shared = Arc::clone(&shared);
        let join = thread::spawn(move || {
            worker_loop(
                loader,
                requests,
                results_sender,
                worker_shared,
                max_items,
                max_bytes,
            );
        });
        Self {
            sender: Some(sender),
            results: Mutex::new(results),
            shared,
            join: Some(join),
            _loader: PhantomData,
        }
    }

    pub fn submit(
        &self,
        request: ExactPromptDiffRequest,
        deadline: Option<Instant>,
    ) -> Result<PromptDiffWorkerSubmission, PromptDiffWorkerError> {
        let _fence = self
            .shared
            .fence
            .lock()
            .map_err(|_| PromptDiffWorkerError::Closed)?;
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(PromptDiffWorkerError::Closed);
        }
        let generation = self
            .shared
            .next_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let request = request.with_generation(generation);
        let cancellation = Arc::new(AtomicBool::new(false));
        let work = WorkerRequest {
            request,
            cancellation: Arc::clone(&cancellation),
            deadline,
        };
        let sender = self.sender.as_ref().ok_or(PromptDiffWorkerError::Closed)?;
        match sender.try_send(work) {
            Ok(()) => {
                update_latest_generation(&self.shared.latest_generation, generation);
                self.shared.cancel_active_except(generation);
                Ok(PromptDiffWorkerSubmission {
                    request,
                    cancellation,
                })
            }
            Err(TrySendError::Full(_)) => Err(PromptDiffWorkerError::QueueFull { generation }),
            Err(TrySendError::Disconnected(_)) => Err(PromptDiffWorkerError::Closed),
        }
    }
}

impl<L> PromptDiffWorker<L> {
    pub fn try_recv(&self) -> Result<Option<PromptDiffWorkerResult>, PromptDiffWorkerError> {
        let results = self
            .results
            .lock()
            .map_err(|_| PromptDiffWorkerError::Closed)?;
        match results.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(PromptDiffWorkerError::Closed),
        }
    }

    pub fn shutdown(&mut self) -> Result<(), PromptDiffWorkerError> {
        {
            let _fence = self
                .shared
                .fence
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.shared.shutdown.store(true, Ordering::Release);
            self.shared.cancel_active();
            self.sender.take();
        }
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        join.join().map_err(|_| PromptDiffWorkerError::JoinFailed)
    }
}

impl<L> Drop for PromptDiffWorker<L> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn update_latest_generation(latest: &AtomicU64, generation: u64) {
    let mut current = latest.load(Ordering::Acquire);
    while current < generation {
        match latest.compare_exchange_weak(current, generation, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

fn worker_loop<L: ExactPromptVersionLoader>(
    loader: L,
    requests: Receiver<WorkerRequest>,
    results: mpsc::Sender<PromptDiffWorkerResult>,
    shared: Arc<WorkerShared>,
    max_items: usize,
    max_bytes: usize,
) {
    let mut service = PromptDiffService::new(loader, max_items, max_bytes);
    while let Ok(work) = requests.recv() {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        shared.set_active(work.request.generation, Arc::clone(&work.cancellation));
        let current_generation = match shared.fence.lock() {
            Ok(_fence) => shared.latest_generation.load(Ordering::Acquire),
            Err(_) => break,
        };
        if work.request.generation != current_generation {
            shared.clear_active();
            continue;
        }
        let outcome =
            service.compute_exact_with_deadline(work.request, &work.cancellation, work.deadline);
        shared.clear_active();
        let Ok(_fence) = shared.fence.lock() else {
            break;
        };
        if shared.shutdown.load(Ordering::Acquire)
            || work.request.generation != shared.latest_generation.load(Ordering::Acquire)
        {
            continue;
        }
        if results
            .send(PromptDiffWorkerResult {
                request: work.request,
                outcome,
            })
            .is_err()
        {
            break;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ServiceCacheKey {
    before_id: PromptVersionId,
    after_id: PromptVersionId,
    before_body_sha256: [u8; 32],
    after_body_sha256: [u8; 32],
    algorithm_version: u16,
    projection_version: u16,
    normalization: super::diff::DiffNormalizationPolicy,
}

struct CachedProjection {
    key: ServiceCacheKey,
    status: DiffStatus,
    bytes: Vec<u8>,
}

/// Crate-local text projection for a native UI renderer.
///
/// This is intentionally not public transport, does not implement serde, and
/// is created only for a bounded worker response. Every nested body-carrying
/// type has a redacted Debug implementation.
#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct LocalPromptDiffProjection {
    status: DiffStatus,
    hunks: Vec<LocalLineHunk>,
    inline_spans: Vec<LocalInlineSpan>,
    estimated_payload_bytes: usize,
    truncation: Option<TruncationMarker>,
}

#[allow(dead_code)]
impl LocalPromptDiffProjection {
    fn from_diff(
        diff: &PromptDiff<'_>,
        cancellation: &AtomicBool,
    ) -> Result<Self, PromptDiffServiceError> {
        let mut copied_text_bytes = 0usize;
        let mut copy = |text: &str| {
            if cancellation.load(Ordering::Relaxed) {
                return Err(PromptDiffServiceError::Cancelled);
            }
            copied_text_bytes = copied_text_bytes.checked_add(text.len()).ok_or(
                PromptDiffServiceError::ProjectionLimit {
                    encoded_bytes: usize::MAX,
                    max_bytes: MAX_PROMPT_DIFF_PAYLOAD_BYTES,
                },
            )?;
            if copied_text_bytes > MAX_PROMPT_DIFF_PAYLOAD_BYTES {
                return Err(PromptDiffServiceError::ProjectionLimit {
                    encoded_bytes: copied_text_bytes,
                    max_bytes: MAX_PROMPT_DIFF_PAYLOAD_BYTES,
                });
            }
            let mut bytes = Vec::with_capacity(text.len());
            for chunk in text.as_bytes().chunks(64) {
                if cancellation.load(Ordering::Relaxed) {
                    return Err(PromptDiffServiceError::Cancelled);
                }
                bytes.extend_from_slice(chunk);
            }
            Ok(String::from_utf8(bytes).expect("diff text is valid UTF-8"))
        };

        let hunks = diff
            .hunks()
            .iter()
            .map(|hunk| local_hunk(hunk, &mut copy))
            .collect::<Result<Vec<_>, _>>()?;
        let inline_spans = diff
            .inline_spans()
            .iter()
            .map(|span| local_inline_span(span, &mut copy))
            .collect::<Result<Vec<_>, _>>()?;
        if cancellation.load(Ordering::Relaxed) {
            return Err(PromptDiffServiceError::Cancelled);
        }
        Ok(Self {
            status: diff.status(),
            hunks,
            inline_spans,
            estimated_payload_bytes: diff.estimated_payload_bytes(),
            truncation: diff.truncation(),
        })
    }

    pub(crate) fn status(&self) -> DiffStatus {
        self.status
    }

    pub(crate) fn hunks(&self) -> &[LocalLineHunk] {
        &self.hunks
    }

    pub(crate) fn inline_spans(&self) -> &[LocalInlineSpan] {
        &self.inline_spans
    }

    pub(crate) fn estimated_payload_bytes(&self) -> usize {
        self.estimated_payload_bytes
    }

    pub(crate) fn truncation(&self) -> Option<TruncationMarker> {
        self.truncation
    }
}

impl fmt::Debug for LocalPromptDiffProjection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalPromptDiffProjection")
            .field("status", &self.status)
            .field("hunk_count", &self.hunks.len())
            .field("inline_span_count", &self.inline_spans.len())
            .field("estimated_payload_bytes", &self.estimated_payload_bytes)
            .field("truncation", &self.truncation)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct LocalLineHunk {
    pub(crate) old_start: usize,
    pub(crate) old_count: usize,
    pub(crate) new_start: usize,
    pub(crate) new_count: usize,
    pub(crate) kind: LineHunkKind,
    pub(crate) changes: Vec<LocalLineChange>,
}

impl fmt::Debug for LocalLineHunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalLineHunk")
            .field("old_start", &self.old_start)
            .field("old_count", &self.old_count)
            .field("new_start", &self.new_start)
            .field("new_count", &self.new_count)
            .field("kind", &self.kind)
            .field("change_count", &self.changes.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct LocalLineChange {
    pub(crate) kind: LineChangeKind,
    pub(crate) old_line: Option<usize>,
    pub(crate) new_line: Option<usize>,
    text: String,
    pub(crate) terminated: bool,
}

impl LocalLineChange {
    #[allow(dead_code)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for LocalLineChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalLineChange")
            .field("kind", &self.kind)
            .field("old_line", &self.old_line)
            .field("new_line", &self.new_line)
            .field("text_bytes", &self.text.len())
            .field("terminated", &self.terminated)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct LocalInlineSpan {
    pub(crate) kind: InlineSpanKind,
    pub(crate) old_line: Option<usize>,
    pub(crate) new_line: Option<usize>,
    pub(crate) old_range: Option<UnicodeRange>,
    pub(crate) new_range: Option<UnicodeRange>,
    text: String,
}

impl LocalInlineSpan {
    #[allow(dead_code)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for LocalInlineSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalInlineSpan")
            .field("kind", &self.kind)
            .field("old_line", &self.old_line)
            .field("new_line", &self.new_line)
            .field("old_range", &self.old_range)
            .field("new_range", &self.new_range)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

fn local_hunk(
    hunk: &LineHunk,
    copy: &mut impl FnMut(&str) -> Result<String, PromptDiffServiceError>,
) -> Result<LocalLineHunk, PromptDiffServiceError> {
    let changes = hunk
        .changes
        .iter()
        .map(|change| {
            Ok(LocalLineChange {
                kind: change.kind(),
                old_line: change.old_line(),
                new_line: change.new_line(),
                text: copy(change.text())?,
                terminated: change.terminated(),
            })
        })
        .collect::<Result<Vec<_>, PromptDiffServiceError>>()?;
    Ok(LocalLineHunk {
        old_start: hunk.old_start,
        old_count: hunk.old_count,
        new_start: hunk.new_start,
        new_count: hunk.new_count,
        kind: hunk.kind,
        changes,
    })
}

fn local_inline_span(
    span: &InlineSpan,
    copy: &mut impl FnMut(&str) -> Result<String, PromptDiffServiceError>,
) -> Result<LocalInlineSpan, PromptDiffServiceError> {
    Ok(LocalInlineSpan {
        kind: span.kind(),
        old_line: span.old_line(),
        new_line: span.new_line(),
        old_range: span.old_range(),
        new_range: span.new_range(),
        text: copy(span.text())?,
    })
}

/// Validated body that exists only for the duration of one actor request.
/// There is no public constructor, Debug, serde implementation, or cache path
/// for this type.
struct BoundedPromptBody {
    #[allow(dead_code)]
    id: PromptVersionId,
    #[allow(dead_code)]
    body_sha256: [u8; 32],
    body: String,
}

impl BoundedPromptBody {
    fn body(&self) -> &str {
        &self.body
    }
}

/// Actor-confined exact-version diff service with an entry- and byte-bounded
/// LRU. It is intentionally private: UI-facing code uses [`PromptDiffWorker`]
/// and cannot synchronously invoke `diff_exact` from paint or input handling.
struct PromptDiffService<L> {
    loader: L,
    max_items: usize,
    max_bytes: usize,
    cache_bytes: usize,
    cache: VecDeque<CachedProjection>,
}

impl<L> PromptDiffService<L> {
    fn new(loader: L, max_items: usize, max_bytes: usize) -> Self {
        Self {
            loader,
            max_items,
            max_bytes,
            cache_bytes: 0,
            cache: VecDeque::new(),
        }
    }
}

impl<L: ExactPromptVersionLoader> PromptDiffService<L> {
    /// Run an exact diff with a host-owned deadline. The same fence is checked
    /// before loading, after loading, after diffing, and immediately before
    /// delivery so an expired worker result cannot reach a newer UI request.
    fn compute_exact_with_deadline(
        &mut self,
        request: ExactPromptDiffRequest,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<PromptDiffServiceResponse, PromptDiffServiceError> {
        self.check_delivery_fence(cancellation, deadline)?;

        let before_metadata = self.load_metadata(
            request.before_id,
            request.before_body_sha256,
            cancellation,
            deadline,
        )?;
        let after_metadata = self.load_metadata(
            request.after_id,
            request.after_body_sha256,
            cancellation,
            deadline,
        )?;
        self.check_delivery_fence(cancellation, deadline)?;
        let before = self.read_validated_body(before_metadata, cancellation, deadline)?;
        self.check_delivery_fence(cancellation, deadline)?;
        let after = self.read_validated_body(after_metadata, cancellation, deadline)?;
        self.check_delivery_fence(cancellation, deadline)?;

        let key = ServiceCacheKey {
            before_id: request.before_id,
            after_id: request.after_id,
            before_body_sha256: request.before_body_sha256,
            after_body_sha256: request.after_body_sha256,
            algorithm_version: PROMPT_DIFF_ALGORITHM_VERSION,
            projection_version: PROMPT_DIFF_PUBLIC_PROJECTION_VERSION,
            normalization: PROMPT_DIFF_NORMALIZATION_POLICY,
        };
        if let Some((status, bytes)) = self.cache_get(key) {
            self.check_delivery_fence(cancellation, deadline)?;
            return Ok(PromptDiffServiceResponse {
                request,
                status,
                public_projection: bytes,
                local_projection: None,
                cache_hit: true,
            });
        }

        let mut budget = DiffBudget::default().with_cancellation(cancellation);
        if let Some(deadline) = deadline {
            budget = budget.with_deadline(deadline);
        }
        let diff = diff_versions_with_budget(before.body(), after.body(), budget);
        let status = diff.status();
        let bytes = encode_public_diff(&diff)?;
        let local_projection = LocalPromptDiffProjection::from_diff(&diff, cancellation)?;
        self.check_delivery_fence(cancellation, deadline)?;

        if status == DiffStatus::Complete {
            self.cache_insert(key, status, bytes.clone());
        }
        self.check_delivery_fence(cancellation, deadline)?;
        Ok(PromptDiffServiceResponse {
            request,
            status,
            public_projection: bytes,
            local_projection: Some(local_projection),
            cache_hit: false,
        })
    }

    fn load_metadata(
        &mut self,
        id: PromptVersionId,
        expected_body_sha256: [u8; 32],
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<ExactPromptVersionMetadata, PromptDiffServiceError> {
        self.check_delivery_fence(cancellation, deadline)?;
        let metadata = self.loader.load_exact_metadata(id)?;
        if metadata.id() != id {
            return Err(PromptDiffServiceError::CorruptVersion { id });
        }
        if metadata.body_bytes() > MAX_PROMPT_BODY_BYTES {
            return Err(PromptDiffServiceError::OversizedVersion {
                id,
                body_bytes: metadata.body_bytes(),
                max_bytes: MAX_PROMPT_BODY_BYTES,
            });
        }
        if expected_body_sha256 != metadata.body_sha256() {
            return Err(PromptDiffServiceError::StaleVersion { id });
        }
        self.check_delivery_fence(cancellation, deadline)?;
        Ok(metadata)
    }

    fn read_validated_body(
        &mut self,
        metadata: ExactPromptVersionMetadata,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<BoundedPromptBody, PromptDiffServiceError> {
        let id = metadata.id();
        self.check_delivery_fence(cancellation, deadline)?;
        let mut writer = PromptVersionBodyWriter::new(metadata.body_bytes());
        self.loader
            .read_exact_body(id, &mut writer, cancellation, deadline)
            .map_err(|error| match error {
                PromptDiffServiceError::BodyBytesExceeded { .. } => {
                    PromptDiffServiceError::CorruptVersion { id }
                }
                error => error,
            })?;
        self.check_delivery_fence(cancellation, deadline)?;
        if writer.bytes_written() != metadata.body_bytes() {
            return Err(PromptDiffServiceError::CorruptVersion { id });
        }
        let body = String::from_utf8(writer.into_bytes())
            .map_err(|_| PromptDiffServiceError::CorruptVersion { id })?;
        if sha256(body.as_bytes()) != metadata.body_sha256() {
            return Err(PromptDiffServiceError::CorruptVersion { id });
        }
        Ok(BoundedPromptBody {
            id,
            body_sha256: metadata.body_sha256(),
            body,
        })
    }

    fn check_delivery_fence(
        &self,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<(), PromptDiffServiceError> {
        if cancellation.load(Ordering::Relaxed) {
            return Err(PromptDiffServiceError::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(PromptDiffServiceError::DeadlineExceeded);
        }
        Ok(())
    }

    fn cache_get(&mut self, key: ServiceCacheKey) -> Option<(DiffStatus, Vec<u8>)> {
        let index = self.cache.iter().position(|entry| entry.key == key)?;
        let entry = self.cache.remove(index)?;
        self.cache_bytes = self.cache_bytes.saturating_sub(entry.bytes.len());
        let status = entry.status;
        let bytes = entry.bytes.clone();
        self.cache_bytes = self.cache_bytes.saturating_add(entry.bytes.len());
        self.cache.push_front(entry);
        Some((status, bytes))
    }

    fn cache_insert(&mut self, key: ServiceCacheKey, status: DiffStatus, bytes: Vec<u8>) {
        if self.max_items == 0 || self.max_bytes == 0 || bytes.len() > self.max_bytes {
            return;
        }
        if let Some(index) = self.cache.iter().position(|entry| entry.key == key) {
            if let Some(entry) = self.cache.remove(index) {
                self.cache_bytes = self.cache_bytes.saturating_sub(entry.bytes.len());
            }
        }
        self.cache_bytes = self.cache_bytes.saturating_add(bytes.len());
        self.cache
            .push_front(CachedProjection { key, status, bytes });
        while self.cache.len() > self.max_items || self.cache_bytes > self.max_bytes {
            if let Some(entry) = self.cache.pop_back() {
                self.cache_bytes = self.cache_bytes.saturating_sub(entry.bytes.len());
            } else {
                break;
            }
        }
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_projection_debug_redacts_nested_text() {
        let cancellation = AtomicBool::new(false);
        let sentinel = "local service body sentinel";
        let diff = super::super::diff::diff_versions(sentinel, "replacement");
        let local = LocalPromptDiffProjection::from_diff(&diff, &cancellation)
            .expect("local projection should fit the bounded result");

        for debug in [
            format!("{local:?}"),
            format!("{:?}", local.hunks()),
            format!("{:?}", local.inline_spans()),
            format!("{:?}", local.hunks()[0].changes),
        ] {
            assert!(!debug.contains(sentinel), "local text leaked: {debug}");
        }
    }
}
