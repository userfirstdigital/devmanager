# Phase 9 Task 9.1 partial report

Status: PARTIAL — Batch1 Rust contract repair is complete in the isolated
`phase-9-1a-wire-catalog` worktree. Batch2-A canonical page-sizing proof is
complete in the isolated `phase-9-1b-page-sizing` worktree. Connect Slice C's
bounded Rust integration is implemented and its focused contracts are green in
the isolated `phase-9-1d-connect-integration` worktree; cross-language/service
gates remain deferred.

## Implemented

- `ConnectEnvelope` is a strict, named MessagePack v1 map containing the
  validated negotiated protocol major/minor, UUIDv7 connection/session/channel
  IDs, channel sequence, optional typed request and operation IDs, negotiated
  physical/reassembled/page/chunk/cumulative/cursor limits, fixed `None`
  compression, privacy class, and typed payload metadata.
- Decode rejects unknown fields, trailing data, positional payload arrays,
  incompatible versions, invalid IDs, zero sequence/version values, unsupported
  compression, invalid limits, metadata mismatches, and oversized payloads.
  Unknown nonzero payload kinds remain inert and cannot become actions.
- One catalog macro in `src/connect/schema.rs` owns all 22 v1 payload tags,
  typed kinds, channel/version/action metadata, and payload bounds. Query and
  QueryReply have distinct discriminants; the committed catalog fixture is
  generated from that production catalog.
- Command receipts and operation settlements are separate replayable payloads.
  A settlement carries an independent operation correlation plus authoritative
  `OperationOutcome`; it does not embed an accepted receipt.
- Limit negotiation and validation cover message, page, chunk, cumulative,
  physical-frame, cursor, and response-page bounds. The committed v1 MessagePack
  fixture proves deterministic encoding and round-trip behavior.
- `ConnectTransport` is a network-free abstraction over the shared inner
  envelope. Direct and relay route metadata do not alter encoded semantics.
- `PermissionEvaluator` covers paired owner, task-scoped watcher, and
  task-scoped collaborator decisions. Watchers cannot mutate; dangerous
  approvals and personal prompt reads remain owner-only; unknown actions deny.
- `PresenceSink` and `EphemeralPresence` retain only bounded, in-memory
  last-sender hints. Presence has no authority, lease, or controller state.
- `ProjectionSource` reuses existing bounded snapshot, replay, query, and
  command-receipt domain types, with optional prompt/browser extension
  descriptors only.
- Connect page validation and outgoing normalization now use the domain
  `canonical_snapshot_page_size`, `canonical_event_page_size`, and
  `canonical_artifact_content_page_size` helpers. Direct and nested QueryReply
  pages are normalized before wire encoding; inbound encoded-byte claims must
  equal the canonical final named-MessagePack size and are never trusted.
- Connect re-exports the reviewed `protocol::ChunkFrame` and
  `protocol::ChunkContext` primitives. The former Connect-local chunk
  constructor, serde, hash, context, poison, and limit algorithms were
  removed; Connect's negotiated chunk/cursor checks delegate to canonical
  `protocol::ChunkLimits` while retaining the envelope limit schema.

## TDD and verification evidence

- RED captured before Batch1 repair: `cargo test --test connect_contract --
  --nocapture` compiled the preserved contract and reported 8 passing tests plus
  one deterministic-catalog failure because the fixture was empty. New RED
  assertions then exposed the missing distinct reply tag, version-bearing
  envelope API, and independent settlement constructor.
- `cargo test --test connect_contract --test protocol_contract --test
  connect_session -- --nocapture`: 11 + 42 + 8 passed.
- `cargo check --lib`: passed.
- `rustfmt --edition 2021 --check` passed for the owned Rust files.
- `cargo fmt --all -- --check` still reports only the preserved salvage
  formatting in `tests/connect_session.rs`; that file was not rewritten.
- Final `git diff --check` and owned-path audit: passed.

## Connect Slice C focused verification

- RED was observed first: the new contract tests rejected the old Connect
  five-argument chunk constructor/signature and the stale outgoing envelope
  page claim with the expected compile/runtime failures.
- `$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase91d-connect'; cargo test --test connect_contract --test connect_session --test protocol_contract -- --nocapture`:
  13 + 8 + 50 passed, 0 failed, 0 ignored.
- The focused run emitted only preserved library warnings. No Cargo, rustc,
  rustdoc, or Rust test-harness process remained for the isolated target after
  completion.
- The hard 18-minute cutoff was reached while the focused gate was running;
  no additional rustfmt, diff-check, full-suite, push, merge, install, tag, or
  publish gate was started afterward. The Slice C worktree remains uncommitted
  pending those intentionally skipped final checks.

## Batch2-A page-sizing verification

- Canonical command: `$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase91b-pages-final'; cargo test --lib 'domain::snapshot::tests' -- --nocapture`.
- Exact result: 8 passed, 0 failed, 0 ignored, 0 measured, and 1,191 filtered.
  This includes 3/3 independent golden-fixture tests, 2/2 MessagePack
  integer-width boundary tests, and 3/3 final named-MessagePack size tests.
- `rustfmt --edition 2021 src/domain/snapshot.rs`: passed. The page slice
  remains test-only in `src/domain/snapshot.rs`; no semantic production or
  Connect edits were made.
- `git diff --check`: passed. The scoped residue check found 0 slice-owned
  Cargo, rustc, or Rust test-harness processes after verification; unrelated
  global compiler processes were left untouched.
- Connect Slice C is explicitly deferred. Batch2-A makes no claim for
  cross-language codec, transport, pairing, relay, or live-service coverage.

## Explicit later gates

This partial does not implement and does not claim to prove:

- paired Rust/TypeScript codec generation, TypeScript fixture decoding, or
  Portal/web protocol tests;
- Noise, stable device/host identity, credential storage, pairing, invites,
  revocation, or task-grant persistence;
- direct WebSocket/LAN transport, Portal relay/routing/presence, or any live
  network/service integration;
- E2E encryption, privacy-boundary probes, cross-language vectors, dependency
  review, or independent security review;
- host/kernel command integration, provider/browser/process/prompt behavior,
  UI, `AppData`, or `session.json` changes;
- migration or porting of legacy `RemoteAction` or `WriterLease` semantics.
- Connect Slice C cross-language codec, pairing, relay, and live-service work
  remain explicitly deferred; the bounded Rust page/chunk integration is
  covered by the focused evidence above.

No live service, installed DevManager, browser, or production persistence was
used or modified.
