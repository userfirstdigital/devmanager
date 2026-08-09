# Phase 9 Task 9.1 partial report

Status: PARTIAL — transport-neutral Rust contract foundation complete in the
isolated `phase-9-1-connect-contract` worktree.

## Implemented

- `ConnectEnvelope` is a strict, named MessagePack v1 map containing protocol
  version, UUIDv7 connection/session/channel IDs, channel sequence, optional
  typed request and operation IDs, negotiated physical/reassembled/page/chunk/
  cumulative limits, fixed `None` compression, privacy class, payload kind and
  version, and binary payload bytes.
- Decode rejects unknown fields, trailing data, positional payload arrays,
  incompatible versions, invalid IDs, zero sequence/version values, unsupported
  compression, invalid limits, and oversized payloads. Unknown nonzero payload
  kinds remain inert and cannot become actions.
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

## TDD and verification evidence

- RED captured before implementation: `cargo test --test connect_session
  wire_ -- --nocapture` first reported that the test target did not exist;
  after the tests were added it failed to compile because `devmanager::connect`
  did not yet exist.
- Additional boundary RED cases were added for zero-capacity presence and
  oversized response cursors before their minimal fixes.
- `cargo test --test connect_session -- --nocapture`: 9 passed.
- `cargo test --test protocol_contract -- --nocapture`: 42 passed.
- `cargo check --lib`: passed.
- `cargo fmt --all -- --check`: passed.
- Final `git diff --check` and owned-path audit: passed.

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

No live service, installed DevManager, browser, or production persistence was
used or modified.
