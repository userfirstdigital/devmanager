# Phase 9 Task 9.1 wire-boundary security report

Status: COMPLETE for the independent Rust Connect wire-boundary findings owned
by the clean `622dfcf` baseline in the isolated
`phase-9-1d-connect-integration` worktree. Cross-language, service, pairing,
relay, and UI gates remain deferred by scope.

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
- Receive derives an effective physical limit as the minimum of negotiated
  physical-frame and reassembled-message budgets. The physical frame codec
  validates that limit before reserving or resizing payload storage.
- Envelope receive performs a minimal named outer decode, compares the
  wire-declared `ConnectLimits` with the negotiated limits, and only then runs
  a borrowed, allocation-free MessagePack wire preflight. The preflight
  rejects negotiated chunk bytes, page item counts/page bytes, cursor arrays or
  binaries, and opaque payload sizes before typed `Vec`/`String` materializes
  or semantic state is mutated.
- Frame/header/partial-I/O, envelope, canonical, negotiated-limit, write, and
  flush failures permanently close `FramedConnectTransport`; subsequent send
  and receive calls return the typed `Closed` error.
- `UnknownPayload` and `GenericExtensionPayload` now have private fields and
  private serde wire DTOs. Checked constructors enforce nonzero kind/version
  metadata and the hard reassembled payload bound; public access is through
  checked getters and the Connect codec boundary.
- `connect_session` now exercises `ConnectEnvelope::encode` and
  `ConnectEnvelope::decode` directly and validates pages through
  `ConnectPayload`, with no removed-helper imports or stale compilation path.

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
- The historical Batch1 owned-file rustfmt check passed; the final full
  `cargo fmt --all -- --check` is recorded below.
- Final `git diff --check` and owned-path audit: passed.

## Wire-boundary security verification

- RED was observed first on the clean `622dfcf` baseline: `connect_session`
  still imported removed `encode_inner`/`decode_inner` helpers, and the new
  boundary assertions exposed allocation-order, declared-limit-order, and
  terminal-close gaps.
- `$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase91d-security-final'; cargo
  test --test connect_contract --test connect_session -- --nocapture`:
  `connect_contract`: 21 passed; `connect_session`: 8 passed; 0 failed,
  0 ignored. This includes exact oversized chunk/page/cursor errors,
  pre-physical-allocation reassembled-frame rejection, declared-limit
  precedence, and partial-I/O/write/flush terminal-closure proofs.
- The focused run emitted only pre-existing library warnings. The isolated
  Cargo/rustc/test-harness process tree was checked after the run and no
  worker-owned Rust processes remained; unrelated global compiler processes
  were left untouched.
- `$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase91d-security-final'; cargo
  check --lib`: passed with only pre-existing library warnings.
- `$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase91d-security-final'; cargo
  test --lib -- --test-threads=1`: 1,198 passed, 1 ignored, 0 failed.
- `cargo fmt --all -- --check` and `git diff --check`: passed. The final
  worktree audit contains only the Connect envelope/schema/transport and
  contract/session test changes plus this report; no `mod.rs` edit was needed.
- The target-scoped residue check found no Cargo, rustc, rustdoc, or Rust
  test-harness process after verification. No installed app, browser,
  provider, profile, UI, live service, or production persistence was used.

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
- The Batch2-A record makes no claim for cross-language codec, pairing, relay,
  or live-service coverage; the bounded Rust wire-boundary follow-up is
  recorded above.

## Explicit later gates

This report does not implement and does not claim to prove:

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
- Cross-language codec, pairing, relay, and live-service work remain explicitly
  deferred; the bounded Rust page/chunk integration is covered by the focused
  evidence above.

No live service, installed DevManager, browser, or production persistence was
used or modified.
