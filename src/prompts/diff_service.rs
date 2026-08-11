//! Host-side exact-version orchestration for bounded prompt diffs.
//!
//! This module deliberately owns no persistence adapter and has no UI/runtime
//! dependencies. A later store adapter supplies [`ExactPromptVersionLoader`],
//! while a host worker calls [`PromptDiffService::diff_exact`] away from paint
//! and input handling. The cache contains only the body-free public projection;
//! any text view is transient in the worker response and is never serialized or
//! retained by the service cache.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::domain::PromptVersionId;

use super::diff::{
    diff_versions_with_budget, encode_public_diff, DiffBudget, DiffStatus, InlineSpan,
    InlineSpanKind, LineChangeKind, LineHunk, LineHunkKind, PromptDiff, PromptDiffEncodeError,
    TruncationMarker, UnicodeRange, MAX_PROMPT_DIFF_PAYLOAD_BYTES, PROMPT_DIFF_ALGORITHM_VERSION,
    PROMPT_DIFF_NORMALIZATION_POLICY, PROMPT_DIFF_PUBLIC_PROJECTION_VERSION,
};

/// An exact immutable version snapshot supplied by a host loader.
///
/// `body` is intentionally private and this type has no serde implementation.
/// The service validates both the supplied fingerprint and the body bytes
/// before passing the body to the bounded diff worker.
#[derive(Clone, PartialEq, Eq)]
pub struct PromptVersionSnapshot {
    id: PromptVersionId,
    body_sha256: [u8; 32],
    body: String,
}

impl PromptVersionSnapshot {
    /// Build a snapshot and derive its fingerprint from the supplied body.
    pub fn from_body(id: PromptVersionId, body: String) -> Self {
        let body_sha256 = sha256(body.as_bytes());
        Self {
            id,
            body_sha256,
            body,
        }
    }

    /// Build a loader snapshot with an independently supplied fingerprint.
    ///
    /// Host adapters normally use [`Self::from_body`]. This constructor keeps
    /// corruption tests and adapters that read a stored digest explicit; the
    /// service rejects a mismatched body before diffing.
    pub fn with_body_sha256(id: PromptVersionId, body: String, body_sha256: [u8; 32]) -> Self {
        Self {
            id,
            body_sha256,
            body,
        }
    }

    pub fn id(&self) -> PromptVersionId {
        self.id
    }

    pub fn body_sha256(&self) -> &[u8; 32] {
        &self.body_sha256
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Debug for PromptVersionSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptVersionSnapshot")
            .field("id", &self.id)
            .field("body_bytes", &self.body.len())
            .field("body_sha256", &self.body_sha256)
            .finish()
    }
}

/// Exact-version loading authority injected by the host/store integration.
///
/// The sole operation takes a requested immutable ID. There is deliberately no
/// `latest`, `current`, cwd, timestamp, or transcript fallback in this trait.
pub trait ExactPromptVersionLoader {
    fn load_exact(
        &self,
        id: PromptVersionId,
    ) -> Result<PromptVersionSnapshot, PromptDiffServiceError>;
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

/// Exact-version diff worker facade with an entry- and byte-bounded LRU.
///
/// `diff_exact` is synchronous by design so an executor can own the call on a
/// host worker. It never touches UI paint/input state and retains only the
/// body-free projection in its cache. The `generation` fence must be advanced
/// by the host before publishing a newly selected request.
pub struct PromptDiffService<L> {
    loader: L,
    max_items: usize,
    max_bytes: usize,
    cache_bytes: usize,
    cache: VecDeque<CachedProjection>,
    generation: u64,
}

impl<L> PromptDiffService<L> {
    pub fn new(loader: L, max_items: usize, max_bytes: usize) -> Self {
        Self {
            loader,
            max_items,
            max_bytes,
            cache_bytes: 0,
            cache: VecDeque::new(),
            generation: 0,
        }
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Advance the delivery fence. Responses for older generations fail even
    /// when their exact IDs happen to be present in the cache.
    pub fn begin_generation(&mut self, generation: u64) {
        self.generation = self.generation.max(generation);
    }
}

impl<L: ExactPromptVersionLoader> PromptDiffService<L> {
    pub fn diff_exact(
        &mut self,
        request: ExactPromptDiffRequest,
        cancellation: &AtomicBool,
    ) -> Result<PromptDiffServiceResponse, PromptDiffServiceError> {
        self.diff_exact_with_deadline(request, cancellation, None)
    }

    /// Run an exact diff with a host-owned deadline. The same fence is checked
    /// before loading, after loading, after diffing, and immediately before
    /// delivery so an expired worker result cannot reach a newer UI request.
    pub fn diff_exact_with_deadline(
        &mut self,
        request: ExactPromptDiffRequest,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<PromptDiffServiceResponse, PromptDiffServiceError> {
        self.check_delivery_fence(request, cancellation, deadline)?;

        let before = self.load_and_validate(request.before_id, request.before_body_sha256)?;
        self.check_delivery_fence(request, cancellation, deadline)?;
        let after = self.load_and_validate(request.after_id, request.after_body_sha256)?;
        self.check_delivery_fence(request, cancellation, deadline)?;

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
            self.check_delivery_fence(request, cancellation, deadline)?;
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
        self.check_delivery_fence(request, cancellation, deadline)?;

        if status == DiffStatus::Complete {
            self.cache_insert(key, status, bytes.clone());
        }
        self.check_delivery_fence(request, cancellation, deadline)?;
        Ok(PromptDiffServiceResponse {
            request,
            status,
            public_projection: bytes,
            local_projection: Some(local_projection),
            cache_hit: false,
        })
    }

    fn load_and_validate(
        &self,
        id: PromptVersionId,
        expected_body_sha256: [u8; 32],
    ) -> Result<PromptVersionSnapshot, PromptDiffServiceError> {
        let snapshot = self.loader.load_exact(id)?;
        if snapshot.id() != id {
            return Err(PromptDiffServiceError::CorruptVersion { id });
        }
        if sha256(snapshot.body().as_bytes()) != *snapshot.body_sha256() {
            return Err(PromptDiffServiceError::CorruptVersion { id });
        }
        if expected_body_sha256 != *snapshot.body_sha256() {
            return Err(PromptDiffServiceError::StaleVersion { id });
        }
        Ok(snapshot)
    }

    fn check_delivery_fence(
        &self,
        request: ExactPromptDiffRequest,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<(), PromptDiffServiceError> {
        if request.generation != self.generation {
            return Err(PromptDiffServiceError::StaleGeneration {
                requested: request.generation,
                current: self.generation,
            });
        }
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
