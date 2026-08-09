use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use similar::{capture_diff_slices, Algorithm, DiffOp, DiffTag, TextDiff};

use super::model::MAX_PROMPT_BODY_BYTES;

/// Maximum number of inline spans retained in a prompt diff.
pub const MAX_PROMPT_DIFF_INLINE_SPANS: usize = 20_000;
/// Maximum estimated encoded size of a prompt diff.
pub const MAX_PROMPT_DIFF_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

const BASE_PAYLOAD_BYTES: usize = 128;
const HUNK_PAYLOAD_BYTES: usize = 96;
const LINE_CHANGE_PAYLOAD_BYTES: usize = 64;
const INLINE_SPAN_PAYLOAD_BYTES: usize = 96;

/// The result of comparing two immutable prompt bodies.
///
/// The operation is pure and should be run by a host background worker, never
/// from prompt input or render code. old_body and new_body always point at the
/// exact caller-provided bytes; CRLF normalization is comparison-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDiff<'a> {
    pub old_body: &'a str,
    pub new_body: &'a str,
    pub old_body_sha256: [u8; 32],
    pub new_body_sha256: [u8; 32],
    pub cache_key: DiffCacheKey,
    pub hunks: Vec<LineHunk>,
    pub inline_spans: Vec<InlineSpan>,
    pub estimated_payload_bytes: usize,
    pub status: DiffStatus,
    pub truncation: Option<TruncationMarker>,
}

/// A typed line-level change hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub kind: LineHunkKind,
    pub changes: Vec<LineChange>,
}

/// The high-level kind of a line hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineHunkKind {
    Added,
    Removed,
    Replaced,
}

/// One line in a hunk. Line numbers are one-based and refer to the original
/// old/new body, not to a normalized comparison buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineChange {
    pub kind: LineChangeKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
    pub terminated: bool,
}

/// The side represented by a line change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineChangeKind {
    Removed,
    Added,
}

/// A bounded Unicode-grapheme inline span within a changed line.
///
/// Ranges use grapheme-cluster indexes, never UTF-8 byte offsets. The span
/// text is copied from the original side and therefore remains valid UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
    pub kind: InlineSpanKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub old_range: Option<UnicodeRange>,
    pub new_range: Option<UnicodeRange>,
    pub text: String,
}

/// The side represented by an inline span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineSpanKind {
    Removed,
    Added,
}

/// A half-open range in Unicode grapheme-cluster units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnicodeRange {
    pub start: usize,
    pub end: usize,
}

/// Why a diff did not retain every output item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationReason {
    InlineSpanLimit,
    PayloadLimit,
}

/// An explicit marker attached whenever an output cap is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncationMarker {
    pub retained_hunks: usize,
    pub retained_inline_spans: usize,
    pub retained_payload_bytes: usize,
    pub reason: TruncationReason,
}

/// The completion state of a diff operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Complete,
    Truncated,
    Cancelled,
    InvalidInput {
        old_bytes: usize,
        new_bytes: usize,
        max_bytes: usize,
    },
}

/// An order-sensitive cache key for a pair of immutable prompt body hashes.
///
/// This slice does not persist or own a cache. A future bounded in-memory LRU
/// can use this key without making SQLite a source of diff truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiffCacheKey {
    pub old_body_sha256: [u8; 32],
    pub new_body_sha256: [u8; 32],
}

impl DiffCacheKey {
    pub fn from_bodies(old: &str, new: &str) -> Self {
        Self {
            old_body_sha256: sha256(old),
            new_body_sha256: sha256(new),
        }
    }
}

/// Cooperative cancellation/deadline inputs for a host background worker.
///
/// similar is intentionally built with only its requested text/unicode/inline
/// features, so the budget is checked before and between every bounded phase.
/// A worker can discard the result immediately after cancellation; the
/// operation never mutates either input.
#[derive(Debug)]
pub struct DiffBudget<'a> {
    deadline: Option<Instant>,
    cancellation: Option<&'a AtomicBool>,
}

impl Default for DiffBudget<'_> {
    fn default() -> Self {
        Self {
            deadline: None,
            cancellation: None,
        }
    }
}

impl<'a> DiffBudget<'a> {
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.deadline = Instant::now().checked_add(timeout);
        self
    }

    pub fn with_cancellation(mut self, cancellation: &'a AtomicBool) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    fn exhausted(&self) -> bool {
        self.cancellation
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// Diff two prompt bodies with the default cooperative budget.
///
/// Call this from a host background worker. Inputs are expected to have
/// already passed the prompt model's 256 KiB body validation.
pub fn diff_versions<'a>(old: &'a str, new: &'a str) -> PromptDiff<'a> {
    diff_versions_with_budget(old, new, DiffBudget::default())
}

/// Diff two prompt bodies while honoring cancellation/deadline checks between
/// bounded comparison and output phases.
pub fn diff_versions_with_budget<'a>(
    old: &'a str,
    new: &'a str,
    budget: DiffBudget<'_>,
) -> PromptDiff<'a> {
    let cache_key = DiffCacheKey::from_bodies(old, new);
    let mut result = PromptDiff {
        old_body: old,
        new_body: new,
        old_body_sha256: cache_key.old_body_sha256,
        new_body_sha256: cache_key.new_body_sha256,
        cache_key,
        hunks: Vec::new(),
        inline_spans: Vec::new(),
        estimated_payload_bytes: base_payload_bytes(old, new),
        status: DiffStatus::Complete,
        truncation: None,
    };

    if old.len() > MAX_PROMPT_BODY_BYTES || new.len() > MAX_PROMPT_BODY_BYTES {
        result.status = DiffStatus::InvalidInput {
            old_bytes: old.len(),
            new_bytes: new.len(),
            max_bytes: MAX_PROMPT_BODY_BYTES,
        };
        return result;
    }
    if budget.exhausted() {
        result.status = DiffStatus::Cancelled;
        return result;
    }

    let old_normalized = normalize_crlf(old);
    let new_normalized = normalize_crlf(new);
    if budget.exhausted() {
        result.status = DiffStatus::Cancelled;
        return result;
    }

    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let old_compare_lines = split_lines(&old_normalized);
    let new_compare_lines = split_lines(&new_normalized);
    debug_assert_eq!(old_lines.len(), old_compare_lines.len());
    debug_assert_eq!(new_lines.len(), new_compare_lines.len());

    let old_keys: Vec<_> = old_compare_lines
        .iter()
        .map(|line| LineKey {
            text: line.text,
            terminated: line.terminated,
        })
        .collect();
    let new_keys: Vec<_> = new_compare_lines
        .iter()
        .map(|line| LineKey {
            text: line.text,
            terminated: line.terminated,
        })
        .collect();

    if budget.exhausted() {
        result.status = DiffStatus::Cancelled;
        return result;
    }

    let ops = capture_diff_slices(Algorithm::Myers, &old_keys, &new_keys);
    let mut output = OutputBudget::new(old, new);

    for op in ops {
        if budget.exhausted() {
            result.status = DiffStatus::Cancelled;
            result.estimated_payload_bytes = output.bytes;
            return result;
        }
        if op.tag() == DiffTag::Equal {
            continue;
        }

        if !output.reserve_hunk() {
            return finish_truncated(result, output);
        }

        let old_range = op.old_range();
        let new_range = op.new_range();
        result.hunks.push(LineHunk {
            old_start: old_range.start + 1,
            old_count: old_range.len(),
            new_start: new_range.start + 1,
            new_count: new_range.len(),
            kind: hunk_kind(&op),
            changes: Vec::new(),
        });
        let hunk_index = result.hunks.len() - 1;

        if !append_line_changes(
            &mut result,
            hunk_index,
            &old_lines,
            &new_lines,
            &old_range,
            &new_range,
            &budget,
            &mut output,
        ) {
            if budget.exhausted() {
                result.status = DiffStatus::Cancelled;
                result.estimated_payload_bytes = output.bytes;
                return result;
            }
            return finish_truncated(result, output);
        }

        if let DiffOp::Replace {
            old_index,
            old_len,
            new_index,
            new_len,
        } = op
        {
            let paired = old_len.min(new_len);
            for offset in 0..paired {
                if budget.exhausted() {
                    result.status = DiffStatus::Cancelled;
                    result.estimated_payload_bytes = output.bytes;
                    return result;
                }
                if !append_inline_spans(
                    &mut result,
                    old_lines[old_index + offset].text,
                    new_lines[new_index + offset].text,
                    old_index + offset + 1,
                    new_index + offset + 1,
                    &budget,
                    &mut output,
                ) {
                    if budget.exhausted() {
                        result.status = DiffStatus::Cancelled;
                        result.estimated_payload_bytes = output.bytes;
                        return result;
                    }
                    return finish_truncated(result, output);
                }
            }
        }
    }

    result.estimated_payload_bytes = output.bytes;
    result.status = DiffStatus::Complete;
    result
}

fn append_line_changes<'a>(
    result: &mut PromptDiff<'a>,
    hunk_index: usize,
    old_lines: &[LineSlice<'a>],
    new_lines: &[LineSlice<'a>],
    old_range: &Range<usize>,
    new_range: &Range<usize>,
    budget: &DiffBudget<'_>,
    output: &mut OutputBudget,
) -> bool {
    for index in old_range.clone() {
        if budget.exhausted() || !output.reserve_line(old_lines[index].text.len()) {
            return false;
        }
        result.hunks[hunk_index].changes.push(LineChange {
            kind: LineChangeKind::Removed,
            old_line: Some(index + 1),
            new_line: None,
            text: old_lines[index].text.to_owned(),
            terminated: old_lines[index].terminated,
        });
    }
    for index in new_range.clone() {
        if budget.exhausted() || !output.reserve_line(new_lines[index].text.len()) {
            return false;
        }
        result.hunks[hunk_index].changes.push(LineChange {
            kind: LineChangeKind::Added,
            old_line: None,
            new_line: Some(index + 1),
            text: new_lines[index].text.to_owned(),
            terminated: new_lines[index].terminated,
        });
    }
    true
}

fn append_inline_spans(
    result: &mut PromptDiff<'_>,
    old_line: &str,
    new_line: &str,
    old_line_number: usize,
    new_line_number: usize,
    budget: &DiffBudget<'_>,
    output: &mut OutputBudget,
) -> bool {
    let diff = TextDiff::configure().diff_graphemes(old_line, new_line);
    for op in diff.ops() {
        if budget.exhausted() || op.tag() == DiffTag::Equal {
            if budget.exhausted() {
                return false;
            }
            continue;
        }

        let old_range = op.old_range();
        let new_range = op.new_range();
        if !append_inline_span(
            result,
            &diff,
            InlineSpanKind::Removed,
            Some(old_line_number),
            Some(new_line_number),
            Some(UnicodeRange {
                start: old_range.start,
                end: old_range.end,
            }),
            None,
            old_range,
            true,
            output,
        ) {
            return false;
        }
        if !append_inline_span(
            result,
            &diff,
            InlineSpanKind::Added,
            Some(old_line_number),
            Some(new_line_number),
            None,
            Some(UnicodeRange {
                start: new_range.start,
                end: new_range.end,
            }),
            new_range,
            false,
            output,
        ) {
            return false;
        }
    }
    true
}

fn append_inline_span<'a>(
    result: &mut PromptDiff<'a>,
    diff: &similar::TextDiff<'_, '_, str>,
    kind: InlineSpanKind,
    old_line: Option<usize>,
    new_line: Option<usize>,
    old_range: Option<UnicodeRange>,
    new_range: Option<UnicodeRange>,
    range: Range<usize>,
    old_side: bool,
    output: &mut OutputBudget,
) -> bool {
    if range.is_empty() {
        return true;
    }
    let text_len = range
        .clone()
        .map(|index| {
            if old_side {
                diff.old_slice(index).map_or(0, |part| part.len())
            } else {
                diff.new_slice(index).map_or(0, |part| part.len())
            }
        })
        .sum();
    if !output.reserve_span(text_len) {
        return false;
    }

    let mut text = String::with_capacity(text_len);
    for index in range {
        let part = if old_side {
            diff.old_slice(index)
        } else {
            diff.new_slice(index)
        };
        if let Some(part) = part {
            text.push_str(part);
        }
    }
    result.inline_spans.push(InlineSpan {
        kind,
        old_line,
        new_line,
        old_range,
        new_range,
        text,
    });
    true
}

fn finish_truncated<'a>(mut result: PromptDiff<'a>, output: OutputBudget) -> PromptDiff<'a> {
    result.estimated_payload_bytes = output.bytes;
    result.status = DiffStatus::Truncated;
    result.truncation = Some(TruncationMarker {
        retained_hunks: result.hunks.len(),
        retained_inline_spans: result.inline_spans.len(),
        retained_payload_bytes: output.bytes,
        reason: output.reason.unwrap_or(TruncationReason::PayloadLimit),
    });
    result
}

fn hunk_kind(op: &DiffOp) -> LineHunkKind {
    match op.tag() {
        DiffTag::Insert => LineHunkKind::Added,
        DiffTag::Delete => LineHunkKind::Removed,
        DiffTag::Replace => LineHunkKind::Replaced,
        DiffTag::Equal => unreachable!("equal operations are not emitted as hunks"),
    }
}

fn base_payload_bytes(old: &str, new: &str) -> usize {
    old.len()
        .saturating_add(new.len())
        .saturating_add(BASE_PAYLOAD_BYTES)
}

fn normalize_crlf(body: &str) -> String {
    body.replace("\r\n", "\n")
}

fn sha256(body: &str) -> [u8; 32] {
    Sha256::digest(body.as_bytes()).into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LineKey<'a> {
    text: &'a str,
    terminated: bool,
}

#[derive(Debug, Clone, Copy)]
struct LineSlice<'a> {
    text: &'a str,
    terminated: bool,
}

fn split_lines(body: &str) -> Vec<LineSlice<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;

    for (index, character) in body.char_indices() {
        if character != '\n' {
            continue;
        }
        let mut text_end = index;
        if text_end > start && body.as_bytes()[text_end - 1] == b'\r' {
            text_end -= 1;
        }
        lines.push(LineSlice {
            text: &body[start..text_end],
            terminated: true,
        });
        start = index + character.len_utf8();
    }

    if start < body.len() {
        lines.push(LineSlice {
            text: &body[start..],
            terminated: false,
        });
    }
    lines
}

struct OutputBudget {
    bytes: usize,
    spans: usize,
    reason: Option<TruncationReason>,
}

impl OutputBudget {
    fn new(old: &str, new: &str) -> Self {
        Self {
            bytes: base_payload_bytes(old, new),
            spans: 0,
            reason: None,
        }
    }

    fn reserve_hunk(&mut self) -> bool {
        self.reserve_bytes(HUNK_PAYLOAD_BYTES)
    }

    fn reserve_line(&mut self, text_bytes: usize) -> bool {
        self.reserve_bytes(LINE_CHANGE_PAYLOAD_BYTES.saturating_add(text_bytes))
    }

    fn reserve_span(&mut self, text_bytes: usize) -> bool {
        if self.spans >= MAX_PROMPT_DIFF_INLINE_SPANS {
            self.reason = Some(TruncationReason::InlineSpanLimit);
            return false;
        }
        if !self.reserve_bytes(INLINE_SPAN_PAYLOAD_BYTES.saturating_add(text_bytes)) {
            return false;
        }
        self.spans += 1;
        true
    }

    fn reserve_bytes(&mut self, bytes: usize) -> bool {
        let Some(next) = self.bytes.checked_add(bytes) else {
            self.reason = Some(TruncationReason::PayloadLimit);
            return false;
        };
        if next > MAX_PROMPT_DIFF_PAYLOAD_BYTES {
            self.reason = Some(TruncationReason::PayloadLimit);
            return false;
        }
        self.bytes = next;
        true
    }
}
