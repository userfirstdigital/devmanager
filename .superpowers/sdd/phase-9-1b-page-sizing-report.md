# Phase 9 Batch2-A: canonical page sizing

Status: GREEN in the isolated `phase-9-1b-page-sizing` worktree.

## Delivered

- Added the domain-owned `CanonicalPageSizeError` with encode, overflow, and
  bounded-nonconvergence variants.
- Added `canonical_snapshot_page_size`, `canonical_event_page_size`, and
  `canonical_artifact_content_page_size` beside the canonical page types.
- Snapshot and artifact sizing now iterate the serialized `encoded_bytes` field
  to a fixed point in at most eight passes. Event sizing measures its canonical
  named MessagePack directly. Overflow and nonconvergence fail closed.
- Snapshot, replay, and artifact kernel paging now delegate sizing to the
  shared domain authority. Cursor HMAC/context checks and artifact response
  headroom policy were not changed.

## TDD and verification evidence

- RED: the four new domain tests failed to compile because the three requested
  helpers were absent.
- GREEN: `cargo test --lib domain::snapshot::tests` — 4 passed.
- Focused kernel tests — 8 passed: snapshot byte-limit, snapshot cursor,
  forged limits, replay byte-limit, replay cursor, artifact exact-byte paging,
  artifact cursor/limits, and artifact cursorless final-page behavior.
- Focused protocol test — 1 passed:
  `pages_recompute_canonical_size_and_do_not_trust_claimed_encoded_bytes`.
- `rustfmt --edition 2021 --check` passed for all owned Rust files.
- `git diff --check` passed.
- Verification used `CARGO_TARGET_DIR=C:\Temp\devmanager-phase91b-pages`.

## Scope and residual risk

- Changed only the five owned Rust files plus this report. Connect catalog and
  chunk code were untouched as required.
- The existing Connect schema snapshot-size validator remains its own
  intentionally untouched Batch2 surface; migrating that validator to this
  authority is a later coordinated change.
- The full repository suite was not run under the 25-minute slice constraint;
  focused coverage passed, with unrelated baseline warnings during integration
  test compilation.
