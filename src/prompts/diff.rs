use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use unicode_segmentation::UnicodeSegmentation;

use super::model::MAX_PROMPT_BODY_BYTES;

/// Maximum number of inline spans retained in a prompt diff.
pub const MAX_PROMPT_DIFF_INLINE_SPANS: usize = 20_000;
/// Maximum estimated encoded size of a prompt diff.
pub const MAX_PROMPT_DIFF_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
/// Conservative encoded-size allowance for a truncation/approximation marker.
pub const PROMPT_DIFF_TRUNCATION_MARKER_BYTES: usize = 256;

const DEFAULT_DIFF_WORK_UNITS: usize = 4 * 1024 * 1024;
const LINE_ANCHOR_WINDOW: usize = 32;
const INLINE_ANCHOR_WINDOW: usize = 32;
const LARGE_HEURISTIC_LINE_COUNT: usize = 512;
const MAX_INLINE_GRAPHEMES: usize = 65_536;
const WORK_CHUNK_BYTES: usize = 64;
const MAX_JSON_NUMBER_BYTES: usize = 20;
const MAX_JSON_BYTE_BYTES: usize = 3;

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

/// Why a diff did not retain every output item or could not prove an exact
/// comparison within its bounded algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationReason {
    InlineSpanLimit,
    PayloadLimit,
    ComplexityLimit,
}

/// An explicit marker attached whenever an output cap or bounded approximation
/// is reached.
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
    /// The result is deterministic but uses a bounded coarse replacement for
    /// a region whose exact alignment would exceed the comparison budget.
    Approximate,
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

/// Cooperative cancellation/deadline and cost inputs for a host background
/// worker.
///
/// The default work limit is deliberately finite. A caller may lower it for a
/// tighter latency budget; reaching it produces a truthful approximate result,
/// while cancellation or deadline expiry produces `Cancelled`.
#[derive(Debug)]
pub struct DiffBudget<'a> {
    deadline: Option<Instant>,
    cancellation: Option<&'a AtomicBool>,
    work_limit: Option<usize>,
    work_used: usize,
    cancel_after_work: Option<usize>,
}

impl Default for DiffBudget<'_> {
    fn default() -> Self {
        Self {
            deadline: None,
            cancellation: None,
            work_limit: Some(DEFAULT_DIFF_WORK_UNITS),
            work_used: 0,
            cancel_after_work: None,
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

    /// Set a deterministic cost ceiling for the comparison phases.
    pub fn with_work_limit(mut self, work_limit: usize) -> Self {
        self.work_limit = Some(work_limit);
        self
    }

    /// Deterministic test/diagnostic helper that flips the supplied token after
    /// the given amount of work has been observed. Host workers normally use
    /// `with_cancellation` and own the token from their executor.
    pub fn with_cancellation_after_work(
        mut self,
        cancellation: &'a AtomicBool,
        work: usize,
    ) -> Self {
        self.cancellation = Some(cancellation);
        self.cancel_after_work = Some(work);
        self
    }

    fn cancellation_or_deadline(&self) -> bool {
        self.cancellation
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn checkpoint(&mut self, units: usize) -> Result<(), BudgetStop> {
        if self.cancellation_or_deadline() {
            return Err(BudgetStop::Cancelled);
        }
        if self.work_limit.is_some_and(|remaining| units > remaining) {
            self.work_limit = Some(0);
            return Err(BudgetStop::WorkLimit);
        }

        if let Some(remaining) = self.work_limit.as_mut() {
            *remaining -= units;
        }
        self.work_used = self.work_used.saturating_add(units);

        if self
            .cancel_after_work
            .is_some_and(|threshold| self.work_used >= threshold)
        {
            if let Some(flag) = self.cancellation {
                flag.store(true, Ordering::Relaxed);
            }
            return Err(BudgetStop::Cancelled);
        }
        Ok(())
    }

    fn observe_cancellation(&self) -> Result<(), BudgetStop> {
        if self.cancellation_or_deadline() {
            Err(BudgetStop::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Diff two prompt bodies with the default bounded cooperative budget.
///
/// Call this from a host background worker. Inputs are expected to have
/// already passed the prompt model's 256 KiB body validation.
pub fn diff_versions<'a>(old: &'a str, new: &'a str) -> PromptDiff<'a> {
    diff_versions_with_budget(old, new, DiffBudget::default())
}

/// Diff two prompt bodies while honoring cancellation, deadline, and work
/// checks in every comparison and materialization phase.
pub fn diff_versions_with_budget<'a>(
    old: &'a str,
    new: &'a str,
    mut budget: DiffBudget<'_>,
) -> PromptDiff<'a> {
    // These guards intentionally precede hashing, result construction, and all
    // comparison-buffer allocation.
    if old.len() > MAX_PROMPT_BODY_BYTES || new.len() > MAX_PROMPT_BODY_BYTES {
        return empty_result(
            old,
            new,
            DiffStatus::InvalidInput {
                old_bytes: old.len(),
                new_bytes: new.len(),
                max_bytes: MAX_PROMPT_BODY_BYTES,
            },
        );
    }
    if budget.cancellation_or_deadline() {
        return empty_result(old, new, DiffStatus::Cancelled);
    }

    let old_body_sha256 = match hash_with_budget(old, &mut budget) {
        Ok(hash) => hash,
        Err(BudgetStop::Cancelled) => return empty_result(old, new, DiffStatus::Cancelled),
        Err(BudgetStop::WorkLimit) => {
            return finish_approximate(
                empty_result(old, new, DiffStatus::Approximate),
                OutputBudget::new(old, new),
            );
        }
    };
    let new_body_sha256 = match hash_with_budget(new, &mut budget) {
        Ok(hash) => hash,
        Err(BudgetStop::Cancelled) => return empty_result(old, new, DiffStatus::Cancelled),
        Err(BudgetStop::WorkLimit) => {
            return finish_approximate(
                empty_result(old, new, DiffStatus::Approximate),
                OutputBudget::new(old, new),
            );
        }
    };

    let cache_key = DiffCacheKey {
        old_body_sha256,
        new_body_sha256,
    };
    let mut result = PromptDiff {
        old_body: old,
        new_body: new,
        old_body_sha256,
        new_body_sha256,
        cache_key,
        hunks: Vec::new(),
        inline_spans: Vec::new(),
        estimated_payload_bytes: base_payload_bytes(old, new),
        status: DiffStatus::Complete,
        truncation: None,
    };
    let mut output = OutputBudget::new(old, new);

    let old_lines = match scan_normalized_lines(old, &mut budget) {
        Ok(lines) => lines,
        Err(BudgetStop::Cancelled) => return finish_cancelled(result, output),
        Err(BudgetStop::WorkLimit) => return finish_approximate(result, output),
    };
    let new_lines = match scan_normalized_lines(new, &mut budget) {
        Ok(lines) => lines,
        Err(BudgetStop::Cancelled) => return finish_cancelled(result, output),
        Err(BudgetStop::WorkLimit) => return finish_approximate(result, output),
    };

    let mut old_index = 0;
    let mut new_index = 0;
    let large_heuristic_input = old_lines.len().max(new_lines.len()) > LARGE_HEURISTIC_LINE_COUNT;
    let mut approximate = false;

    while old_index < old_lines.len() && new_index < new_lines.len() {
        match lines_equal(&old_lines[old_index], &new_lines[new_index], &mut budget) {
            Ok(true) => {
                old_index += 1;
                new_index += 1;
                continue;
            }
            Ok(false) => {}
            Err(BudgetStop::Cancelled) => return finish_cancelled(result, output),
            Err(BudgetStop::WorkLimit) => {
                approximate = true;
                break;
            }
        }

        let old_start = old_index;
        let new_start = new_index;
        approximate |= large_heuristic_input;
        match find_line_anchor(&old_lines, &new_lines, old_start, new_start, &mut budget) {
            Ok(Some((anchor_old, anchor_new))) => {
                match append_hunk(
                    &mut result,
                    &old_lines,
                    &new_lines,
                    old_start..anchor_old,
                    new_start..anchor_new,
                    &mut budget,
                    &mut output,
                ) {
                    Ok(was_approximate) => approximate |= was_approximate,
                    Err(PhaseStop::Cancelled) => return finish_cancelled(result, output),
                    Err(PhaseStop::WorkLimit) => {
                        approximate = true;
                        break;
                    }
                    Err(PhaseStop::OutputLimit) => return finish_truncated(result, output),
                }
                old_index = anchor_old;
                new_index = anchor_new;
            }
            Ok(None) => {
                let remaining_old = old_lines.len() - old_start;
                let remaining_new = new_lines.len() - new_start;
                if remaining_old.max(remaining_new) > LINE_ANCHOR_WINDOW {
                    approximate = true;
                }
                match append_hunk(
                    &mut result,
                    &old_lines,
                    &new_lines,
                    old_start..old_lines.len(),
                    new_start..new_lines.len(),
                    &mut budget,
                    &mut output,
                ) {
                    Ok(was_approximate) => approximate |= was_approximate,
                    Err(PhaseStop::Cancelled) => return finish_cancelled(result, output),
                    Err(PhaseStop::WorkLimit) => approximate = true,
                    Err(PhaseStop::OutputLimit) => return finish_truncated(result, output),
                }
                old_index = old_lines.len();
                new_index = new_lines.len();
            }
            Err(BudgetStop::Cancelled) => return finish_cancelled(result, output),
            Err(BudgetStop::WorkLimit) => {
                approximate = true;
                match append_hunk(
                    &mut result,
                    &old_lines,
                    &new_lines,
                    old_start..old_lines.len(),
                    new_start..new_lines.len(),
                    &mut budget,
                    &mut output,
                ) {
                    Ok(_) | Err(PhaseStop::WorkLimit) => {}
                    Err(PhaseStop::Cancelled) => return finish_cancelled(result, output),
                    Err(PhaseStop::OutputLimit) => return finish_truncated(result, output),
                }
                old_index = old_lines.len();
                new_index = new_lines.len();
            }
        }
    }

    if old_index < old_lines.len() || new_index < new_lines.len() {
        approximate |= large_heuristic_input;
        match append_hunk(
            &mut result,
            &old_lines,
            &new_lines,
            old_index..old_lines.len(),
            new_index..new_lines.len(),
            &mut budget,
            &mut output,
        ) {
            Ok(was_approximate) => approximate |= was_approximate,
            Err(PhaseStop::Cancelled) => return finish_cancelled(result, output),
            Err(PhaseStop::WorkLimit) => approximate = true,
            Err(PhaseStop::OutputLimit) => return finish_truncated(result, output),
        }
    }

    if let Err(BudgetStop::Cancelled) = budget.observe_cancellation() {
        return finish_cancelled(result, output);
    }
    if approximate {
        finish_approximate(result, output)
    } else {
        result.estimated_payload_bytes = output.bytes;
        result.status = DiffStatus::Complete;
        result
    }
}

fn empty_result<'a>(old: &'a str, new: &'a str, status: DiffStatus) -> PromptDiff<'a> {
    PromptDiff {
        old_body: old,
        new_body: new,
        old_body_sha256: [0; 32],
        new_body_sha256: [0; 32],
        cache_key: DiffCacheKey {
            old_body_sha256: [0; 32],
            new_body_sha256: [0; 32],
        },
        hunks: Vec::new(),
        inline_spans: Vec::new(),
        estimated_payload_bytes: base_payload_bytes(old, new).min(MAX_PROMPT_DIFF_PAYLOAD_BYTES),
        status,
        truncation: None,
    }
}

fn hash_with_budget(body: &str, budget: &mut DiffBudget<'_>) -> Result<[u8; 32], BudgetStop> {
    let mut hasher = Sha256::new();
    for chunk in body.as_bytes().chunks(4 * 1024) {
        budget.checkpoint(chunk.len())?;
        hasher.update(chunk);
    }
    Ok(hasher.finalize().into())
}

fn scan_normalized_lines<'a>(
    body: &'a str,
    budget: &mut DiffBudget<'_>,
) -> Result<Vec<LineSlice<'a>>, BudgetStop> {
    let bytes = body.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;

    for (index, &byte) in bytes.iter().enumerate() {
        if index % WORK_CHUNK_BYTES == 0 {
            budget.checkpoint((bytes.len() - index).min(WORK_CHUNK_BYTES))?;
        }
        if byte != b'\n' {
            continue;
        }
        let mut text_end = index;
        if text_end > start && bytes[text_end - 1] == b'\r' {
            text_end -= 1;
        }
        lines.push(LineSlice {
            text: &body[start..text_end],
            terminated: true,
        });
        start = index + 1;
    }

    if start < body.len() {
        budget.checkpoint((body.len() - start).min(WORK_CHUNK_BYTES))?;
        lines.push(LineSlice {
            text: &body[start..],
            terminated: false,
        });
    }
    Ok(lines)
}

fn lines_equal(
    old: &LineSlice<'_>,
    new: &LineSlice<'_>,
    budget: &mut DiffBudget<'_>,
) -> Result<bool, BudgetStop> {
    if old.terminated != new.terminated || old.text.len() != new.text.len() {
        return Ok(false);
    }
    for (old_chunk, new_chunk) in old
        .text
        .as_bytes()
        .chunks(WORK_CHUNK_BYTES)
        .zip(new.text.as_bytes().chunks(WORK_CHUNK_BYTES))
    {
        budget.checkpoint(old_chunk.len())?;
        if old_chunk != new_chunk {
            return Ok(false);
        }
    }
    Ok(true)
}

fn find_line_anchor(
    old_lines: &[LineSlice<'_>],
    new_lines: &[LineSlice<'_>],
    old_start: usize,
    new_start: usize,
    budget: &mut DiffBudget<'_>,
) -> Result<Option<(usize, usize)>, BudgetStop> {
    let old_end = (old_start + LINE_ANCHOR_WINDOW).min(old_lines.len());
    let new_end = (new_start + LINE_ANCHOR_WINDOW).min(new_lines.len());
    let old_span = old_end - old_start;
    let new_span = new_end - new_start;
    let max_score = old_span
        .saturating_sub(1)
        .saturating_add(new_span.saturating_sub(1));

    for score in 1..=max_score {
        let old_offset_min = score.saturating_sub(new_span.saturating_sub(1));
        let old_offset_max = score.min(old_span.saturating_sub(1));
        for old_offset in old_offset_min..=old_offset_max {
            let new_offset = score - old_offset;
            if lines_equal(
                &old_lines[old_start + old_offset],
                &new_lines[new_start + new_offset],
                budget,
            )? {
                return Ok(Some((old_start + old_offset, new_start + new_offset)));
            }
        }
    }
    Ok(None)
}

fn append_hunk<'a>(
    result: &mut PromptDiff<'a>,
    old_lines: &[LineSlice<'a>],
    new_lines: &[LineSlice<'a>],
    old_range: Range<usize>,
    new_range: Range<usize>,
    budget: &mut DiffBudget<'_>,
    output: &mut OutputBudget,
) -> Result<bool, PhaseStop> {
    if old_range.is_empty() && new_range.is_empty() {
        return Ok(false);
    }
    if !output.reserve_hunk() {
        return Err(PhaseStop::OutputLimit);
    }

    let hunk_index = result.hunks.len();
    result.hunks.push(LineHunk {
        old_start: old_range.start + 1,
        old_count: old_range.len(),
        new_start: new_range.start + 1,
        new_count: new_range.len(),
        kind: line_hunk_kind(old_range.len(), new_range.len()),
        changes: Vec::new(),
    });

    append_line_changes(
        result, hunk_index, old_lines, new_lines, &old_range, &new_range, budget, output,
    )?;

    let paired = old_range.len().min(new_range.len());
    let mut approximate = false;
    for offset in 0..paired {
        match append_inline_spans(
            result,
            old_lines[old_range.start + offset].text,
            new_lines[new_range.start + offset].text,
            old_range.start + offset + 1,
            new_range.start + offset + 1,
            budget,
            output,
        )? {
            InlineOutcome::Exact => {}
            InlineOutcome::Approximate => approximate = true,
        }
    }
    Ok(approximate)
}

fn append_line_changes<'a>(
    result: &mut PromptDiff<'a>,
    hunk_index: usize,
    old_lines: &[LineSlice<'a>],
    new_lines: &[LineSlice<'a>],
    old_range: &Range<usize>,
    new_range: &Range<usize>,
    budget: &mut DiffBudget<'_>,
    output: &mut OutputBudget,
) -> Result<(), PhaseStop> {
    for index in old_range.clone() {
        budget
            .observe_cancellation()
            .map_err(|_| PhaseStop::Cancelled)?;
        if !output.reserve_line(old_lines[index].text) {
            return Err(PhaseStop::OutputLimit);
        }
        result.hunks[hunk_index].changes.push(LineChange {
            kind: LineChangeKind::Removed,
            old_line: Some(index + 1),
            new_line: None,
            text: copy_with_cancellation(old_lines[index].text, budget)?,
            terminated: old_lines[index].terminated,
        });
    }
    for index in new_range.clone() {
        budget
            .observe_cancellation()
            .map_err(|_| PhaseStop::Cancelled)?;
        if !output.reserve_line(new_lines[index].text) {
            return Err(PhaseStop::OutputLimit);
        }
        result.hunks[hunk_index].changes.push(LineChange {
            kind: LineChangeKind::Added,
            old_line: None,
            new_line: Some(index + 1),
            text: copy_with_cancellation(new_lines[index].text, budget)?,
            terminated: new_lines[index].terminated,
        });
    }
    Ok(())
}

fn append_inline_spans(
    result: &mut PromptDiff<'_>,
    old_line: &str,
    new_line: &str,
    old_line_number: usize,
    new_line_number: usize,
    budget: &mut DiffBudget<'_>,
    output: &mut OutputBudget,
) -> Result<InlineOutcome, PhaseStop> {
    let old_graphemes = collect_graphemes(old_line, budget).map_err(PhaseStop::from)?;
    let new_graphemes = collect_graphemes(new_line, budget).map_err(PhaseStop::from)?;
    let mut old_index = 0;
    let mut new_index = 0;
    let mut approximate = false;

    while old_index < old_graphemes.len() && new_index < new_graphemes.len() {
        match graphemes_equal(&old_graphemes[old_index], &new_graphemes[new_index], budget)
            .map_err(PhaseStop::from)?
        {
            true => {
                old_index += 1;
                new_index += 1;
                continue;
            }
            false => {}
        }

        let old_start = old_index;
        let new_start = new_index;
        let anchor =
            find_grapheme_anchor(&old_graphemes, &new_graphemes, old_start, new_start, budget)
                .map_err(PhaseStop::from)?;
        let (old_end, new_end) = anchor.unwrap_or((old_graphemes.len(), new_graphemes.len()));
        if anchor.is_none()
            && (old_graphemes.len() - old_start).max(new_graphemes.len() - new_start)
                > LINE_ANCHOR_WINDOW
        {
            approximate = true;
        }

        append_inline_span_pair(
            result,
            old_line,
            new_line,
            old_line_number,
            new_line_number,
            &old_graphemes,
            &new_graphemes,
            old_start..old_end,
            new_start..new_end,
            budget,
            output,
        )?;
        old_index = old_end;
        new_index = new_end;
    }

    if old_index < old_graphemes.len() || new_index < new_graphemes.len() {
        append_inline_span_pair(
            result,
            old_line,
            new_line,
            old_line_number,
            new_line_number,
            &old_graphemes,
            &new_graphemes,
            old_index..old_graphemes.len(),
            new_index..new_graphemes.len(),
            budget,
            output,
        )?;
    }

    Ok(if approximate {
        InlineOutcome::Approximate
    } else {
        InlineOutcome::Exact
    })
}

fn collect_graphemes<'a>(
    line: &'a str,
    budget: &mut DiffBudget<'_>,
) -> Result<Vec<GraphemeSlice<'a>>, BudgetStop> {
    let mut graphemes = Vec::new();
    for (start, text) in line.grapheme_indices(true) {
        if graphemes.len() >= MAX_INLINE_GRAPHEMES {
            return Err(BudgetStop::WorkLimit);
        }
        budget.checkpoint(text.len())?;
        graphemes.push(GraphemeSlice {
            text,
            range: start..start + text.len(),
        });
    }
    Ok(graphemes)
}

fn graphemes_equal(
    old: &GraphemeSlice<'_>,
    new: &GraphemeSlice<'_>,
    budget: &mut DiffBudget<'_>,
) -> Result<bool, BudgetStop> {
    if old.text.len() != new.text.len() {
        return Ok(false);
    }
    for (old_chunk, new_chunk) in old
        .text
        .as_bytes()
        .chunks(WORK_CHUNK_BYTES)
        .zip(new.text.as_bytes().chunks(WORK_CHUNK_BYTES))
    {
        budget.checkpoint(old_chunk.len())?;
        if old_chunk != new_chunk {
            return Ok(false);
        }
    }
    Ok(true)
}

fn find_grapheme_anchor(
    old_graphemes: &[GraphemeSlice<'_>],
    new_graphemes: &[GraphemeSlice<'_>],
    old_start: usize,
    new_start: usize,
    budget: &mut DiffBudget<'_>,
) -> Result<Option<(usize, usize)>, BudgetStop> {
    let old_end = (old_start + INLINE_ANCHOR_WINDOW).min(old_graphemes.len());
    let new_end = (new_start + INLINE_ANCHOR_WINDOW).min(new_graphemes.len());
    let old_span = old_end - old_start;
    let new_span = new_end - new_start;
    let max_score = old_span
        .saturating_sub(1)
        .saturating_add(new_span.saturating_sub(1));

    for score in 1..=max_score {
        let old_offset_min = score.saturating_sub(new_span.saturating_sub(1));
        let old_offset_max = score.min(old_span.saturating_sub(1));
        for old_offset in old_offset_min..=old_offset_max {
            let new_offset = score - old_offset;
            if graphemes_equal(
                &old_graphemes[old_start + old_offset],
                &new_graphemes[new_start + new_offset],
                budget,
            )? {
                return Ok(Some((old_start + old_offset, new_start + new_offset)));
            }
        }
    }
    Ok(None)
}

fn append_inline_span_pair(
    result: &mut PromptDiff<'_>,
    old_line: &str,
    new_line: &str,
    old_line_number: usize,
    new_line_number: usize,
    old_graphemes: &[GraphemeSlice<'_>],
    new_graphemes: &[GraphemeSlice<'_>],
    old_range: Range<usize>,
    new_range: Range<usize>,
    budget: &DiffBudget<'_>,
    output: &mut OutputBudget,
) -> Result<(), PhaseStop> {
    if !old_range.is_empty() {
        append_inline_span(
            result,
            old_line,
            old_line_number,
            new_line_number,
            old_graphemes,
            old_range,
            InlineSpanKind::Removed,
            budget,
            output,
        )?;
    }
    if !new_range.is_empty() {
        append_inline_span(
            result,
            new_line,
            old_line_number,
            new_line_number,
            new_graphemes,
            new_range,
            InlineSpanKind::Added,
            budget,
            output,
        )?;
    }
    Ok(())
}

fn append_inline_span(
    result: &mut PromptDiff<'_>,
    line: &str,
    old_line_number: usize,
    new_line_number: usize,
    graphemes: &[GraphemeSlice<'_>],
    range: Range<usize>,
    kind: InlineSpanKind,
    budget: &DiffBudget<'_>,
    output: &mut OutputBudget,
) -> Result<(), PhaseStop> {
    if range.is_empty() {
        return Ok(());
    }
    let start = graphemes[range.start].range.start;
    let end = graphemes[range.end - 1].range.end;
    let text = &line[start..end];
    budget
        .observe_cancellation()
        .map_err(|_| PhaseStop::Cancelled)?;
    if !output.reserve_span(text) {
        return Err(PhaseStop::OutputLimit);
    }
    result.inline_spans.push(InlineSpan {
        kind,
        old_line: Some(old_line_number),
        new_line: Some(new_line_number),
        old_range: (kind == InlineSpanKind::Removed).then_some(UnicodeRange {
            start: range.start,
            end: range.end,
        }),
        new_range: (kind == InlineSpanKind::Added).then_some(UnicodeRange {
            start: range.start,
            end: range.end,
        }),
        text: copy_with_cancellation(text, budget)?,
    });
    Ok(())
}

fn copy_with_cancellation(text: &str, budget: &DiffBudget<'_>) -> Result<String, PhaseStop> {
    budget
        .observe_cancellation()
        .map_err(|_| PhaseStop::Cancelled)?;
    let mut bytes = Vec::with_capacity(text.len());
    for chunk in text.as_bytes().chunks(WORK_CHUNK_BYTES) {
        budget
            .observe_cancellation()
            .map_err(|_| PhaseStop::Cancelled)?;
        bytes.extend_from_slice(chunk);
    }
    Ok(String::from_utf8(bytes).expect("copied bytes came from a valid UTF-8 str"))
}

fn line_hunk_kind(old_count: usize, new_count: usize) -> LineHunkKind {
    match (old_count, new_count) {
        (0, _) => LineHunkKind::Added,
        (_, 0) => LineHunkKind::Removed,
        _ => LineHunkKind::Replaced,
    }
}

fn finish_cancelled<'a>(mut result: PromptDiff<'a>, output: OutputBudget) -> PromptDiff<'a> {
    result.estimated_payload_bytes = output.bytes.min(MAX_PROMPT_DIFF_PAYLOAD_BYTES);
    result.status = DiffStatus::Cancelled;
    result.truncation = None;
    result
}

fn finish_truncated<'a>(mut result: PromptDiff<'a>, output: OutputBudget) -> PromptDiff<'a> {
    let estimated_payload_bytes = output.bytes_with_marker();
    result.estimated_payload_bytes = estimated_payload_bytes;
    result.status = DiffStatus::Truncated;
    result.truncation = Some(TruncationMarker {
        retained_hunks: result.hunks.len(),
        retained_inline_spans: result.inline_spans.len(),
        retained_payload_bytes: estimated_payload_bytes,
        reason: output.reason.unwrap_or(TruncationReason::PayloadLimit),
    });
    result
}

fn finish_approximate<'a>(mut result: PromptDiff<'a>, output: OutputBudget) -> PromptDiff<'a> {
    let estimated_payload_bytes = output.bytes_with_marker();
    result.estimated_payload_bytes = estimated_payload_bytes;
    result.status = DiffStatus::Approximate;
    result.truncation = Some(TruncationMarker {
        retained_hunks: result.hunks.len(),
        retained_inline_spans: result.inline_spans.len(),
        retained_payload_bytes: estimated_payload_bytes,
        reason: TruncationReason::ComplexityLimit,
    });
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineSlice<'a> {
    text: &'a str,
    terminated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphemeSlice<'a> {
    text: &'a str,
    range: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetStop {
    Cancelled,
    WorkLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseStop {
    Cancelled,
    WorkLimit,
    OutputLimit,
}

impl From<BudgetStop> for PhaseStop {
    fn from(stop: BudgetStop) -> Self {
        match stop {
            BudgetStop::Cancelled => Self::Cancelled,
            BudgetStop::WorkLimit => Self::WorkLimit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineOutcome {
    Exact,
    Approximate,
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
        self.reserve_bytes(1usize.saturating_add(json_hunk_bytes()))
    }

    fn reserve_line(&mut self, text: &str) -> bool {
        self.reserve_bytes(1usize.saturating_add(json_line_change_bytes(text)))
    }

    fn reserve_span(&mut self, text: &str) -> bool {
        if self.spans >= MAX_PROMPT_DIFF_INLINE_SPANS {
            self.reason = Some(TruncationReason::InlineSpanLimit);
            return false;
        }
        if !self.reserve_bytes(1usize.saturating_add(json_inline_span_bytes(text))) {
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

    fn bytes_with_marker(&self) -> usize {
        self.bytes.min(MAX_PROMPT_DIFF_PAYLOAD_BYTES)
    }
}

fn json_string_bytes(value: &str) -> usize {
    value.bytes().fold(2usize, |bytes, byte| {
        bytes.saturating_add(match byte {
            b'"' | b'\\' => 2,
            b'\x08' | b'\x09' | b'\x0a' | b'\x0c' | b'\x0d' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
    })
}

fn json_field_bytes(name: &str, value_bytes: usize) -> usize {
    json_string_bytes(name)
        .saturating_add(1)
        .saturating_add(value_bytes)
}

fn json_object_bytes(fields: &[usize]) -> usize {
    2usize
        .saturating_add(fields.iter().copied().sum::<usize>())
        .saturating_add(fields.len().saturating_sub(1))
}

fn json_array_bytes(items: &[usize]) -> usize {
    2usize
        .saturating_add(items.iter().copied().sum::<usize>())
        .saturating_add(items.len().saturating_sub(1))
}

fn json_hash_bytes() -> usize {
    json_array_bytes(&[MAX_JSON_BYTE_BYTES; 32])
}

fn json_range_bytes() -> usize {
    json_object_bytes(&[
        json_field_bytes("start", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("end", MAX_JSON_NUMBER_BYTES),
    ])
}

fn json_status_bytes() -> usize {
    let invalid_input = json_object_bytes(&[json_field_bytes(
        "InvalidInput",
        json_object_bytes(&[
            json_field_bytes("old_bytes", MAX_JSON_NUMBER_BYTES),
            json_field_bytes("new_bytes", MAX_JSON_NUMBER_BYTES),
            json_field_bytes("max_bytes", MAX_JSON_NUMBER_BYTES),
        ]),
    )]);
    [
        json_string_bytes("Complete"),
        json_string_bytes("Approximate"),
        json_string_bytes("Truncated"),
        json_string_bytes("Cancelled"),
        invalid_input,
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
}

fn json_truncation_marker_bytes() -> usize {
    let marker = json_object_bytes(&[
        json_field_bytes("retained_hunks", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("retained_inline_spans", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("retained_payload_bytes", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("reason", json_string_bytes("ComplexityLimit")),
    ]);
    debug_assert!(marker <= PROMPT_DIFF_TRUNCATION_MARKER_BYTES);
    marker
}

fn base_payload_bytes(old: &str, new: &str) -> usize {
    json_object_bytes(&[
        json_field_bytes("old_body", json_string_bytes(old)),
        json_field_bytes("new_body", json_string_bytes(new)),
        json_field_bytes("old_body_sha256", json_hash_bytes()),
        json_field_bytes("new_body_sha256", json_hash_bytes()),
        json_field_bytes(
            "cache_key",
            json_object_bytes(&[
                json_field_bytes("old_body_sha256", json_hash_bytes()),
                json_field_bytes("new_body_sha256", json_hash_bytes()),
            ]),
        ),
        json_field_bytes("hunks", json_array_bytes(&[])),
        json_field_bytes("inline_spans", json_array_bytes(&[])),
        json_field_bytes("estimated_payload_bytes", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("status", json_status_bytes()),
        json_field_bytes("truncation", json_truncation_marker_bytes()),
    ])
}

fn json_hunk_bytes() -> usize {
    json_object_bytes(&[
        json_field_bytes("old_start", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("old_count", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("new_start", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("new_count", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("kind", json_string_bytes("Replaced")),
        json_field_bytes("changes", json_array_bytes(&[])),
    ])
}

fn json_line_change_bytes(text: &str) -> usize {
    json_object_bytes(&[
        json_field_bytes("kind", json_string_bytes("Removed")),
        json_field_bytes("old_line", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("new_line", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("text", json_string_bytes(text)),
        json_field_bytes("terminated", 5),
    ])
}

fn json_inline_span_bytes(text: &str) -> usize {
    json_object_bytes(&[
        json_field_bytes("kind", json_string_bytes("Removed")),
        json_field_bytes("old_line", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("new_line", MAX_JSON_NUMBER_BYTES),
        json_field_bytes("old_range", json_range_bytes()),
        json_field_bytes("new_range", json_range_bytes()),
        json_field_bytes("text", json_string_bytes(text)),
    ])
}
