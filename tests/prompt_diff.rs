use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use devmanager::prompts::{
    diff_versions, diff_versions_with_budget, DiffBudget, DiffCacheKey, DiffStatus, InlineSpanKind,
    LineChangeKind, MAX_PROMPT_BODY_BYTES, MAX_PROMPT_DIFF_INLINE_SPANS,
    MAX_PROMPT_DIFF_PAYLOAD_BYTES,
};

#[test]
fn identical_versions_have_no_changes_and_retain_original_bodies() {
    let body = "same\r\n東京 🙂\n";

    let diff = diff_versions(body, body);

    assert_eq!(diff.status, DiffStatus::Complete);
    assert_eq!(diff.old_body, body);
    assert_eq!(diff.new_body, body);
    assert!(diff.hunks.is_empty());
    assert!(diff.inline_spans.is_empty());
}

#[test]
fn line_hunks_type_add_remove_replace_and_keep_stable_line_numbers() {
    let old = "keep\nremove\nreplace old\n";
    let new = "keep\nreplace new\nadded\n";

    let diff = diff_versions(old, new);
    let changes: Vec<_> = diff.hunks.iter().flat_map(|hunk| &hunk.changes).collect();

    assert!(changes.iter().any(|change| {
        change.kind == LineChangeKind::Removed
            && change.old_line == Some(2)
            && change.text == "remove"
    }));
    assert!(changes.iter().any(|change| {
        change.kind == LineChangeKind::Removed
            && change.old_line == Some(3)
            && change.text == "replace old"
    }));
    assert!(changes.iter().any(|change| {
        change.kind == LineChangeKind::Added
            && change.new_line == Some(2)
            && change.text == "replace new"
    }));
    assert!(changes.iter().any(|change| {
        change.kind == LineChangeKind::Added && change.new_line == Some(3) && change.text == "added"
    }));
}

#[test]
fn inline_spans_use_unicode_safe_ranges_and_original_new_line_numbers() {
    let old = "Café 東京 😀\n";
    let new = "Café 東京 😃\n";

    let diff = diff_versions(old, new);

    assert!(!diff.inline_spans.is_empty());
    assert!(diff.inline_spans.iter().all(|span| {
        span.old_line == Some(1)
            && span.new_line == Some(1)
            && span.text.is_char_boundary(0)
            && span.text.is_char_boundary(span.text.len())
            && span
                .old_range
                .as_ref()
                .is_none_or(|range| range.start <= range.end)
            && span
                .new_range
                .as_ref()
                .is_none_or(|range| range.start <= range.end)
    }));
    assert!(diff
        .inline_spans
        .iter()
        .any(|span| span.kind == InlineSpanKind::Removed));
    assert!(diff
        .inline_spans
        .iter()
        .any(|span| span.kind == InlineSpanKind::Added));
}

#[test]
fn crlf_and_lf_compare_equal_without_mutating_bodies() {
    let old = String::from("one\r\ntwo\r\n");
    let new = String::from("one\ntwo\n");
    let old_before = old.clone();
    let new_before = new.clone();

    let diff = diff_versions(&old, &new);

    assert_eq!(diff.status, DiffStatus::Complete);
    assert_eq!(diff.old_body, old_before);
    assert_eq!(diff.new_body, new_before);
    assert_eq!(old, old_before);
    assert_eq!(new, new_before);
    assert!(diff.hunks.is_empty());
}

#[test]
fn no_final_newline_is_a_typed_line_change() {
    let diff = diff_versions("line\n", "line");
    let changes: Vec<_> = diff.hunks.iter().flat_map(|hunk| &hunk.changes).collect();

    assert!(changes.iter().any(|change| {
        change.kind == LineChangeKind::Removed && change.old_line == Some(1) && change.terminated
    }));
    assert!(changes.iter().any(|change| {
        change.kind == LineChangeKind::Added && change.new_line == Some(1) && !change.terminated
    }));
}

#[test]
fn empty_versions_are_deterministic_add_and_remove_operations() {
    let added = diff_versions("", "new\n");
    let removed = diff_versions("old\n", "");
    let unchanged = diff_versions("", "");

    assert!(added
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.changes)
        .any(|change| change.kind == LineChangeKind::Added && change.new_line == Some(1)));
    assert!(removed
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.changes)
        .any(|change| change.kind == LineChangeKind::Removed && change.old_line == Some(1)));
    assert!(unchanged.hunks.is_empty());
}

#[test]
fn move_like_content_has_stable_repeatable_output() {
    let old = "A\nB\nC\n";
    let new = "B\nC\nA\n";

    let first = diff_versions(old, new);
    let second = diff_versions(old, new);

    assert_eq!(first, second);
}

#[test]
fn long_unicode_lines_remain_valid_and_bounded() {
    let old = format!("prefix {} suffix", "🙂界é".repeat(20_000));
    let new = old.replace("prefix", "changed");

    let diff = diff_versions(&old, &new);

    assert_eq!(diff.old_body, old);
    assert_eq!(diff.new_body, new);
    assert!(diff
        .inline_spans
        .iter()
        .all(|span| span.text.is_char_boundary(0) && span.text.is_char_boundary(span.text.len())));
    assert!(diff.inline_spans.len() <= MAX_PROMPT_DIFF_INLINE_SPANS);
    assert!(diff.estimated_payload_bytes <= MAX_PROMPT_DIFF_PAYLOAD_BYTES);
}

#[test]
fn exact_prompt_body_bound_is_accepted_and_larger_input_is_rejected() {
    let body = "x".repeat(MAX_PROMPT_BODY_BYTES);
    let exact = diff_versions(&body, &body);
    assert_eq!(exact.status, DiffStatus::Complete);

    let too_large = format!("{body}x");
    let rejected = diff_versions(&body, &too_large);
    assert!(matches!(rejected.status, DiffStatus::InvalidInput { .. }));
    assert_eq!(rejected.old_body, body);
    assert_eq!(rejected.new_body, too_large);
}

#[test]
fn pathological_many_line_output_stops_at_both_output_caps() {
    let old = "a\n".repeat(50_000);
    let new = "b\n".repeat(50_000);

    let diff = diff_versions(&old, &new);

    assert!(diff.inline_spans.len() <= MAX_PROMPT_DIFF_INLINE_SPANS);
    assert!(diff.estimated_payload_bytes <= MAX_PROMPT_DIFF_PAYLOAD_BYTES);
    assert!(diff.truncation.is_some());
    let marker = diff.truncation.expect("large diff must carry a marker");
    assert_eq!(marker.retained_inline_spans, diff.inline_spans.len());
    assert_eq!(marker.retained_hunks, diff.hunks.len());
    assert_eq!(marker.retained_payload_bytes, diff.estimated_payload_bytes);
}

#[test]
fn cancellation_and_expired_deadline_return_bounded_results() {
    let cancelled = AtomicBool::new(true);
    let cancelled_old = "old\n".repeat(100);
    let cancelled_new = "new\n".repeat(100);
    let cancelled_diff = diff_versions_with_budget(
        &cancelled_old,
        &cancelled_new,
        DiffBudget::default().with_cancellation(&cancelled),
    );
    assert_eq!(cancelled_diff.status, DiffStatus::Cancelled);
    assert!(cancelled_diff.hunks.is_empty());

    let expired_old = "old\n".repeat(100);
    let expired_new = "new\n".repeat(100);
    let expired_diff = diff_versions_with_budget(
        &expired_old,
        &expired_new,
        DiffBudget::default().with_deadline(Instant::now() - Duration::from_secs(1)),
    );
    assert_eq!(expired_diff.status, DiffStatus::Cancelled);
    assert!(expired_diff.hunks.is_empty());
}

#[test]
fn cache_keys_are_sha256_based_and_order_sensitive() {
    let old_new = DiffCacheKey::from_bodies("old", "new");
    let new_old = DiffCacheKey::from_bodies("new", "old");

    assert_ne!(old_new, new_old);
    assert_ne!(old_new.old_body_sha256, old_new.new_body_sha256);
    assert_eq!(old_new.old_body_sha256, sha256(b"old"));
    assert_eq!(old_new.new_body_sha256, sha256(b"new"));
}

#[test]
fn input_strings_are_not_changed_by_diffing() {
    let old = String::from("before\r\n東京\n");
    let new = String::from("after\n東京\n");
    let old_before = old.clone();
    let new_before = new.clone();

    let _ = diff_versions(&old, &new);

    assert_eq!(old, old_before);
    assert_eq!(new, new_before);
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes).into()
}
