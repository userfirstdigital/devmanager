# Phase 1: Domain, Store, and Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish the durable, presentation-independent task model, SQLite event store, idempotent accepted/settled operation path, paged snapshots, and bounded versioned wire protocol that every later host and client uses.

**Architecture:** Domain facts use typed UUIDv7 identifiers and pure deterministic transitions. Each accepted command commits its operation record, events, read projections, command receipt, and side-effect outbox records in one SQLite transaction; acceptance never claims the side effect settled. Clients exchange negotiated bounded MessagePack frames and recover from paged snapshots plus ordered events. Large messages use resumable chunks with strict physical/reassembled limits; neither GPUI nor web types are allowed in this layer.

**Tech Stack:** Rust 1.94.0, serde, rmp-serde, rusqlite 0.39.0 with bundled SQLite, uuid 1.24.0 with v7/serde, tempfile.

## Global Constraints

- This phase may not launch PTYs, providers, browsers, services, or UI windows.
- `TaskId`, `AgentSessionId`, `ArtifactId`, `ResourceId`, `ClientId`, `CommandId`, and `EventId` are distinct types, never strings or interchangeable UUID aliases.
- Facts are append-only. Projections are disposable and reproducible from facts.
- A retry with the same `CommandId` returns the stored receipt without appending another event or outbox item.
- One accepted command is one SQLite transaction: events, projections, receipt, and outbox either all commit or none commit.
- Every accepted command has one durable `OperationId`. Pure database work may settle in the acceptance transaction; side-effecting work emits one later generation-fenced `Settled`, `Failed`, `Cancelled`, or `Uncertain` fact.
- External exactly-once delivery is not a valid invariant. Every outbox effect declares `RetrySafe`, `ReconcileBeforeRetry`, or `NoAutomaticRetry`; after an ambiguous dispatch boundary, non-retry-safe work becomes visible `Uncertain` instead of being sent again automatically.
- Optimistic concurrency uses `expected_task_revision`; a mismatch is a typed rejection, not a retry or silent overwrite.
- `config.json` and `remote.json` remain direct supported contracts. The new kernel must never read or infer state from `session.json`.
- Wire decoding is bounded before allocation. V1 hard ceilings are 1 MiB per physical frame, 16 MiB per reassembled message, 1,000 items/512 KiB per page, and 256 KiB per chunk; peers advertise lower limits when required.
- Unknown optional capabilities/frames are ignored or surfaced through the closed generic extension envelope; incompatible protocol majors fail visibly. Unknown data never becomes a domain transition.
- Use the Phase 0 isolated worktree throughout. Pure domain/protocol tests and tests with explicit temporary roots may use direct focused Cargo red/green loops; profile-backed configuration tests and the grouped phase exit use exact named recipes with production/process evidence. Never restore generic command or argument admission.

---

## File map

- Create: `src/domain/mod.rs`
- Create: `src/domain/id.rs`
- Create: `src/domain/task.rs`
- Create: `src/domain/agent.rs`
- Create: `src/domain/artifact.rs`
- Create: `src/domain/resource.rs`
- Create: `src/domain/command.rs`
- Create: `src/domain/event.rs`
- Create: `src/domain/operation.rs`
- Create: `src/domain/query.rs`
- Create: `src/domain/snapshot.rs`
- Create: `src/kernel/mod.rs`
- Create: `src/kernel/schema.rs`
- Create: `src/kernel/store.rs`
- Create: `src/kernel/command_bus.rs`
- Create: `src/kernel/projector.rs`
- Create: `src/kernel/outbox.rs`
- Create: `src/kernel/runtime.rs`
- Create: `src/protocol/mod.rs`
- Create: `src/protocol/capabilities.rs`
- Create: `src/protocol/envelope.rs`
- Create: `src/protocol/frame.rs`
- Create: `src/protocol/chunk.rs`
- Create: `src/config/model.rs`
- Create: `src/config/project_store.rs`
- Create: `src/config/remote_store.rs`
- Modify: `src/config/mod.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Create: `tests/domain_identity.rs`
- Create: `tests/task_state.rs`
- Create: `tests/kernel_store.rs`
- Create: `tests/protocol_contract.rs`
- Create: `tests/operation_lifecycle.rs`
- Create: `tests/configuration_contract.rs`
- Modify: `scripts/native-next/PhaseGate.ps1` (add the exact Phase 1 cargo recipes)
- Modify: `tests/development_isolation.rs` (lock the Phase 1 recipe vectors and reject generic arguments)

### Task 1.1: Add typed identifiers and immutable task facts

**Files:** `Cargo.toml`, `src/domain/{mod,id,task,agent,artifact,resource,operation}.rs`, `src/lib.rs`, `tests/domain_identity.rs`

**Contracts:**

```rust
pub struct TaskId(Uuid);
pub struct AgentSessionId(Uuid);
pub struct ArtifactId(Uuid);
pub struct ResourceId(Uuid);
pub struct ClientId(Uuid);
pub struct OperationId(Uuid);
pub struct TransferId(Uuid);

pub struct TaskFacts {
    pub id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
    pub assignment: TaskAssignment,
    pub lifecycle: TaskLifecycle,
    pub action_epoch: u64,
    pub revision: u64,
    pub created_at_ms: i64,
}
```

- [ ] **Step 1: Write the failing identity tests** in `tests/domain_identity.rs` for UUIDv7 generation, serde round-trip, display/parse round-trip, invalid-version rejection, and compile-time non-interchangeability using `static_assertions::assert_not_impl_any!`.
- [ ] **Step 2: Run** `cargo test --test domain_identity -- --nocapture` directly in the isolated worktree and retain the unresolved `devmanager::domain` failure. This test is pure and has no persistence/runtime/process surface.
- [ ] **Step 3: Add** `rusqlite = { version = "0.39.0", features = ["bundled"] }`; retain the Phase 0 `uuid = { version = "1.24.0", features = ["v7", "serde"] }`; implement a private `define_id!` macro whose public types validate UUID version 7 at every external decoding boundary, including text, bytes, and serde.
- [ ] **Step 4: Define** `EnvironmentId`, `ProjectId`, `TaskId`, `AgentSessionId`, `ArtifactId`, `ResourceId`, `TerminalId`, `BrowserContextId`, `ServiceId`, `ClientId`, `CommandId`, `RequestId`, `OperationId`, `TransferId`, `SubscriptionId`, and `EventId`; expose UUID bytes for persistence without exposing cross-type conversion.
- [ ] **Step 5: Define** `WorkspaceRef::{Main, Worktree { path, branch }, External { path }}`, `TaskLifecycle::{Open, Closing, Archived}`, `TaskAssignment::{LocalOwner, ExternalPrincipal { authority, subject }}`, and fact records for task, agent, artifact, and resource ownership. Validate titles, descriptions, principal references, and paths at construction boundaries.
- [ ] **Step 6: Run** `cargo test --test domain_identity -- --nocapture` and confirm all identity tests pass.
- [ ] **Step 7: Commit** with `git add Cargo.toml Cargo.lock src/domain src/lib.rs tests/domain_identity.rs && git commit -m "feat(kernel): define durable domain identities"`.

### Task 1.2: Define versioned commands, events, receipts, and pure task transitions

**Files:** `src/domain/{command,event,query,task,snapshot}.rs`, `tests/task_state.rs`

**Contracts:**

```rust
pub struct CommandEnvelope {
    pub command_id: CommandId,
    pub client_id: ClientId,
    pub task_id: Option<TaskId>,
    pub issued_at_ms: i64,
    pub expected_task_revision: Option<u64>,
    pub command: Command,
}

pub enum CommandReceipt {
    Accepted {
        command_id: CommandId,
        operation_id: OperationId,
        task_revision: Option<u64>,
        event_ids: Vec<EventId>,
    },
    Rejected { command_id: CommandId, code: RejectionCode, current_revision: Option<u64> },
}

pub enum OperationState {
    Accepted,
    Settled { settled_at_ms: i64, result_event_ids: Vec<EventId> },
    Failed { settled_at_ms: i64, code: OperationErrorCode },
    Cancelled { settled_at_ms: i64, reason: CancellationReason },
    Uncertain { observed_at_ms: i64, code: OperationUncertaintyCode },
}

pub struct QueryEnvelope {
    pub request_id: RequestId,
    pub client_id: ClientId,
    pub task_id: Option<TaskId>,
    pub query: Query,
}

pub enum Query {
    OperationStatus { operation_id: OperationId },
}

pub enum QueryResult {
    OperationStatus { operation_id: OperationId, state: OperationState },
}

pub enum QueryError {
    NotFound,
    Unauthorized,
    InvalidRequest,
    UnsupportedCapability,
}

pub struct QueryReply {
    pub request_id: RequestId,
    pub outcome: QueryOutcome,
}

pub enum QueryOutcome {
    Ok(QueryResult),
    Err(QueryError),
}

pub struct DomainEvent {
    pub id: EventId,
    pub task_id: Option<TaskId>,
    pub sequence: u64,
    pub task_revision: Option<u64>,
    pub occurred_at_ms: i64,
    pub payload: Event,
}
```

- [ ] **Step 1: Write failing tests** named `create_task_emits_revision_one`, `rename_requires_expected_revision`, `closing_is_idempotent`, `closing_advances_action_epoch`, `archived_task_rejects_new_runtime`, `agent_and_resource_must_reference_same_task`, `visible_status_precedence_is_deterministic`, `accepted_side_effect_is_not_settled`, and `replay_derives_identical_snapshot`.
- [ ] **Step 2: Run** `cargo test --test task_state -- --nocapture` and retain the missing command/event types as the expected red result.
- [ ] **Step 3: Define the initial mutation commands:** `CreateTask`, `RenameTask`, `SetTaskAttention`, `BeginCloseTask`, `ReopenTask`, `RegisterAgentSession`, `SetPrimaryAgent`, `RegisterArtifact`, `RegisterResource`, and `ReleaseResource`. Each carries complete intent; commands may not contain UI indexes or labels as identity. `BeginCloseTask` atomically moves `Open -> Closing` and increments `action_epoch` before any teardown side effect is dispatched. Define side-effect-free `Query::OperationStatus { operation_id }` and its typed result separately so checking settlement never creates another operation.
- [ ] **Step 4: Define matching facts** plus separate axes `TaskConnectivity`, `TaskAttention::{None, NeedsAnswer, NeedsApproval, UncertainOutcome, Failed}`, `TaskActivity::{Idle, Working, Settling}`, `ReviewReadiness::{NotReady, Ready}`, and typed rejection codes: `NotFound`, `AlreadyExists`, `RevisionConflict`, `InvalidTransition`, `OwnershipConflict`, and `UnsupportedCapability`.
- [ ] **Step 5: Implement** `decide(snapshot, command) -> Result<Vec<Event>, RejectionCode>` and pure `apply(snapshot, event)`. Derive `VisibleTaskStatus` with precedence disconnected, failed, uncertain outcome, needs approval, needs answer, working/settling, ready for review, idle; never store it as a second mutable truth.
- [ ] **Step 6: Make event serialization explicit** with `{ event_type, schema_version, payload }`; include `OperationAccepted`, `OperationSettled`, `OperationFailed`, `OperationCancelled`, and `OperationUncertain` facts correlated to command/operation IDs and generation/action epoch. `OperationAccepted` is the durable acceptance marker that lets the operations projection rebuild solely from ordered events without claiming settlement; its task scope comes only from `DomainEvent.task_id` (the accepted payload carries no `task_id`), and Task 1.3 must derive `operations.task_id` from that wrapper. Add golden JSON fixtures so renaming a Rust enum cannot silently change the durable format.
- [ ] **Step 7: Run** `cargo test --test task_state -- --nocapture`; then commit with `git commit -am "feat(kernel): define task commands and events"` after staging new files.

### Task 1.3: Create SQLite schema v1 and deterministic projections

**Files:** `src/kernel/{mod,schema,projector}.rs`, `tests/kernel_store.rs`

**Schema:**

```sql
CREATE TABLE schema_migrations (
  version INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  applied_at_ms INTEGER NOT NULL,
  sha256 BLOB NOT NULL CHECK(length(sha256) = 32)
);
CREATE TABLE events (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id BLOB NOT NULL UNIQUE CHECK(length(event_id) = 16),
  task_id BLOB,
  task_revision INTEGER,
  event_type TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  occurred_at_ms INTEGER NOT NULL,
  payload BLOB NOT NULL
);
CREATE TABLE command_receipts (
  command_id BLOB PRIMARY KEY CHECK(length(command_id) = 16),
  client_id BLOB NOT NULL CHECK(length(client_id) = 16),
  task_id BLOB,
  receipt BLOB NOT NULL,
  committed_sequence INTEGER,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE operations (
  operation_id BLOB PRIMARY KEY CHECK(length(operation_id) = 16),
  command_id BLOB NOT NULL UNIQUE REFERENCES command_receipts(command_id) DEFERRABLE INITIALLY DEFERRED,
  task_id BLOB,
  action_epoch INTEGER,
  runtime_generation INTEGER,
  state TEXT NOT NULL,
  result BLOB,
  outcome_code TEXT,
  accepted_at_ms INTEGER NOT NULL,
  outcome_at_ms INTEGER
);
CREATE TABLE tasks (
  task_id BLOB PRIMARY KEY CHECK(length(task_id) = 16),
  environment_id BLOB NOT NULL CHECK(length(environment_id) = 16),
  project_id BLOB NOT NULL CHECK(length(project_id) = 16),
  title TEXT NOT NULL,
  description TEXT,
  workspace BLOB NOT NULL,
  assignment BLOB NOT NULL,
  lifecycle TEXT NOT NULL,
  action_epoch INTEGER NOT NULL,
  revision INTEGER NOT NULL,
  connectivity TEXT NOT NULL,
  attention TEXT NOT NULL,
  activity TEXT NOT NULL,
  review_readiness TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE agent_sessions (
  agent_session_id BLOB PRIMARY KEY CHECK(length(agent_session_id) = 16),
  task_id BLOB NOT NULL REFERENCES tasks(task_id),
  role BLOB NOT NULL,
  provider_kind TEXT NOT NULL,
  provider_session_id TEXT,
  lifecycle TEXT NOT NULL,
  runtime_generation INTEGER NOT NULL,
  revision INTEGER NOT NULL
);
CREATE TABLE artifacts (
  artifact_id BLOB PRIMARY KEY CHECK(length(artifact_id) = 16),
  task_id BLOB NOT NULL REFERENCES tasks(task_id),
  kind TEXT NOT NULL,
  label TEXT NOT NULL,
  content_ref BLOB NOT NULL,
  sha256 BLOB NOT NULL CHECK(length(sha256) = 32),
  privacy_class TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE resources (
  resource_id BLOB PRIMARY KEY CHECK(length(resource_id) = 16),
  task_id BLOB REFERENCES tasks(task_id),
  owner_kind TEXT NOT NULL,
  resource_kind TEXT NOT NULL,
  recipe BLOB NOT NULL,
  lifecycle TEXT NOT NULL,
  runtime_generation INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE outbox (
  outbox_id BLOB PRIMARY KEY CHECK(length(outbox_id) = 16),
  operation_id BLOB NOT NULL REFERENCES operations(operation_id),
  effect_index INTEGER NOT NULL CHECK(effect_index >= 0),
  event_sequence INTEGER NOT NULL REFERENCES events(sequence),
  destination_class TEXT NOT NULL,
  replay_policy TEXT NOT NULL,
  payload BLOB NOT NULL,
  state TEXT NOT NULL,
  available_at_ms INTEGER NOT NULL,
  leased_until_ms INTEGER,
  dispatch_started_at_ms INTEGER,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error_class TEXT,
  UNIQUE(operation_id, effect_index)
);
```

- [ ] **Step 1: Add failing store tests** for an empty migration, WAL mode, foreign-key enforcement, schema-version rejection, projection rebuild, corrupt/truncated database, interrupted transaction, and integrity-check failure. Assert exact table/index names with `sqlite_schema`.
- [ ] **Step 2: Run** `cargo test --test kernel_store schema_ -- --nocapture` and observe the missing schema module.
- [ ] **Step 3: Implement** ordered migrations in `schema_migrations`; open file databases with WAL, `foreign_keys=ON`, a bounded busy timeout, and `synchronous=NORMAL`; use full durability for explicit checkpoint/export operations.
- [ ] **Step 4: Store identifiers** as fixed 16-byte blobs, timestamps as Unix milliseconds, event/receipt/operation payloads as MessagePack, and human-queryable discriminants as stable text. Add indexes for task sequence, task revision, operation state, outbox delivery state, operation/effect identity, and active resources. V1 keeps compact command receipts for permanent idempotency; it does not expire a known `CommandId` into a second mutation.
- [ ] **Step 5: Implement projectors** for `tasks`, `operations`, `agent_sessions`, `artifacts`, and `resources`. Projection functions accept a transaction and one event and contain no clocks, random generation, filesystem calls, or network calls.
- [ ] **Step 6: Implement `rebuild_projections`** into shadow tables, compare deterministic rows, then swap inside one transaction; this is a repair/verification tool, not normal startup work.
- [ ] **Step 7: Run** `cargo test --test kernel_store schema_ -- --nocapture`; commit as `feat(kernel): add sqlite event schema and projections`.

### Task 1.4: Make command execution atomic and idempotent

**Files:** `src/kernel/{store,command_bus,outbox}.rs`, `tests/kernel_store.rs`

**Contract:** `KernelStore::execute(CommandEnvelope) -> CommandReceipt` performs lookup, revision validation, operation allocation, decision, append, projection, receipt, and outbox insert in one immediate transaction. `KernelStore::record_outcome(OperationOutcome) -> OperationState` is a separate idempotent generation/action-epoch-fenced transaction for side-effect success, failure, cancellation, or uncertainty.

- [ ] **Step 1: Add failing tests** `accepted_command_commits_operation_and_all_records`, `accepted_side_effect_remains_accepted_until_outcome`, `pure_database_command_settles_in_acceptance_transaction`, `projector_failure_rolls_back_every_record`, `duplicate_command_returns_original_operation`, `revision_conflict_persists_only_rejection_receipt`, `duplicate_rejected_command_stays_rejected`, `stale_generation_cannot_record_outcome`, `duplicate_outcome_returns_original_state`, `retry_safe_effect_reuses_external_idempotency_key`, `reconcile_before_retry_proves_absence`, `ambiguous_non_retryable_effect_becomes_uncertain`, `uncertain_effect_is_not_auto_dispatched_again`, and `concurrent_writers_accept_only_one_revision`.
- [ ] **Step 2: Run** `cargo test --test kernel_store command_ -- --nocapture` and save the red output.
- [ ] **Step 3: Implement `KernelStore`** with a single writer connection owned by the kernel executor and read-only snapshot connections. No caller receives a raw writable `rusqlite::Connection`.
- [ ] **Step 4: Start an immediate transaction**, return an existing receipt/operation before decision when `command_id` is known, load the current projection, compare `expected_task_revision`, and decide. Persist a final rejection receipt without operation/events/projection/outbox; the same CommandId can never become accepted on retry.
- [ ] **Step 5: For acceptance, allocate all IDs, write the receipt and its deferred-linked `Accepted` operation row, allocate event sequence, apply projections, and insert one outbox row per required side effect using stable `(operation_id, effect_index)` identity plus an explicit replay policy**, then commit. Pure database commands also write their `OperationSettled` fact in this transaction. Convert constraint, corruption, and busy failures into distinct `StoreError` variants; never claim settlement merely because acceptance committed.
- [ ] **Step 6: Implement dispatch/outcome rules.** `RetrySafe` reuses the same external idempotency key; `ReconcileBeforeRetry` may retry only after stable external identity proves the effect absent; `NoAutomaticRetry` records dispatch start before crossing the external boundary and becomes `Uncertain` after an ambiguous crash/timeout. Only the current operation action epoch/runtime generation may move `Accepted` to `Settled`, `Failed`, `Cancelled`, or `Uncertain`; a verified reconciliation may later resolve `Uncertain` to `Settled`/`Failed`, but never re-dispatches it implicitly. Duplicate identical outcomes are idempotent and conflicting/stale outcomes are rejected with evidence.
- [ ] **Step 7: Add a deterministic failpoint used only by tests** between event insertion and projection so the rollback test proves atomicity rather than merely checking the happy path.
- [ ] **Step 8: Run** `cargo test --test kernel_store command_ -- --nocapture`; commit as `feat(kernel): execute and settle operations atomically`.

### Task 1.5: Add snapshots, ordered replay, and runtime generation fences

**Files:** `src/domain/snapshot.rs`, `src/kernel/{store,runtime}.rs`, `tests/kernel_store.rs`

- [ ] **Step 1: Write failing tests** for `snapshot_has_global_cursor`, `snapshot_pages_resume_without_duplicates`, `snapshot_page_honors_item_and_encoded_byte_limits`, `events_after_cursor_are_strictly_ordered`, `expired_cursor_requires_snapshot`, `operation_status_survives_reopen`, `runtime_generation_rejects_stale_completion`, and `archived_task_has_no_live_resources`.
- [ ] **Step 2: Run** `cargo test --test kernel_store replay_ -- --nocapture` and record the red result.
- [ ] **Step 3: Define** `SnapshotPage { snapshot_id, through_sequence, section, after_item, items, encoded_bytes, next_cursor }` and `EventPage { after_sequence, through_sequence, events, next_cursor }`. Pages stop before either 1,000 items or 512 KiB encoded, return an opaque HMAC-bound cursor, and keep ephemeral terminal grids/screenshots/large artifacts outside the durable snapshot.
- [ ] **Step 4: Add retention metadata** and a typed `ReplayUnavailable { oldest_sequence, newest_sequence }`; a client must request a fresh paged snapshot instead of guessing across a gap. Cursors bind snapshot ID, through-sequence, section, last item, and negotiated page limits so they cannot be replayed against another snapshot/limit set.
- [ ] **Step 5: Implement `RuntimeRegistry`** keyed by typed resource ID with monotonically increasing generation. Every asynchronous completion carries `(resource_id, generation)` and stale completions are discarded.
- [ ] **Step 6: Add bounded store maintenance** for WAL checkpoints, settled outbox payload cleanup while retaining compact effect/receipt idempotency records, integrity checks, and optional event-retention snapshots. Canonical facts and known command IDs are never pruned without an explicit retention/compaction policy and a replay boundary.
- [ ] **Step 7: On recovery**, load durable resource recipes as `Recovering` facts, reconcile actual processes in later phases, and never convert stored metadata into a claim that a process is still alive.
- [ ] **Step 8: Run** the focused replay/runtime/maintenance tests and commit as `feat(kernel): add paged snapshots replay and generation fences`.

### Task 1.6: Define capability negotiation and bounded MessagePack framing

**Files:** `src/protocol/{mod,capabilities,envelope,frame,chunk}.rs`, `tests/protocol_contract.rs`, `tests/fixtures/protocol/v1/*`

**Wire handshake:**

```rust
pub struct ClientHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub client_build: String,
    pub client_id: ClientId,
    pub requested: CapabilitySet,
    pub limits: FrameLimits,
}

pub enum ClientMessage {
    Hello(ClientHello),
    Command(CommandEnvelope),
    Query(QueryEnvelope),
    Subscribe(SubscriptionRequest),
    Unsubscribe { subscription_id: SubscriptionId },
    Ack { scope: AckScope, through_sequence: u64 },
}

pub enum ServerMessage {
    Hello(ServerHello),
    Receipt(CommandReceipt),
    QueryReply(QueryReply),
    SnapshotPage(SnapshotPage),
    Events(EventPage),
    Stream(StreamFrame),
    Chunk(ChunkFrame),
    Extension(GenericExtensionFrame),
    ResyncRequired { scope: ResyncScope, reason: ResyncReason },
    Error(ProtocolError),
}

pub struct StreamFrame {
    pub subscription_id: SubscriptionId,
    pub stream: StreamKey,
    pub generation: u64,
    pub sequence: u64,
    pub payload_kind: StreamPayloadKind,
    pub schema_version: u16,
    pub payload: Vec<u8>,
}

pub struct FrameLimits {
    pub max_physical_frame_bytes: u32,
    pub max_reassembled_message_bytes: u32,
    pub max_page_items: u32,
    pub max_page_encoded_bytes: u32,
}

pub struct ChunkFrame {
    pub transfer_id: TransferId,
    pub index: u32,
    pub final_chunk: bool,
    pub payload: Vec<u8>,
    pub cumulative_sha256: [u8; 32],
    pub resume_cursor: Option<Vec<u8>>,
}
```

- [ ] **Step 1: Write failing protocol tests** for byte-for-byte command/receipt/query/reply/snapshot-page/event/stream/chunk/extension golden frames, fragmented reads, coalesced frames, zero-length rejection, header rejection above 1 MiB before allocation, negotiation to the lower peer limits, 16 MiB reassembly ceiling, 256 KiB chunk ceiling, cumulative-hash mismatch, duplicate/out-of-order/resumed chunks, 1,000-item/512 KiB page limits, scoped stream/global resync, unknown minor capability/extension tolerance, major-version rejection, and malformed MessagePack closure.
- [ ] **Step 2: Run** `cargo test --test protocol_contract -- --nocapture` and record the missing protocol failure.
- [ ] **Step 3: Define** protocol major/minor constants and named capabilities for paged snapshots, event replay, operation settlement, chunk resume, generic extensions, semantic conversation, terminal deltas, browser projection, prompt projection, Connect encryption, guests, and management metadata. `FrameLimits::v1_default()` is exactly 1 MiB physical, 16 MiB reassembled, 1,000 page items, and 512 KiB encoded page; peers use the per-field minimum.
- [ ] **Step 4: Implement** an unsigned 32-bit big-endian length prefix. Reject zero/oversized lengths from the four-byte header before reserving payload memory; read exactly one frame; deserialize with depth/collection/string bounds. A peer cannot negotiate above the local hard ceilings.
- [ ] **Step 5: Implement chunk transfer** at 256 KiB maximum payload with transfer identity, contiguous index, cumulative SHA-256, negotiated total reassembly budget, cancellation/expiry, and opaque resume cursor. Store file/artifact chunks directly in their bounded destination rather than concatenating arbitrary history in RAM; a mismatched hash/index discards the transfer and emits a closed error.
- [ ] **Step 6: Add request correlation and subscription semantics**: mutations correlate by `CommandId`, side-effect-free queries by `RequestId`, accepted work by `OperationId`, and subscriptions by `SubscriptionId`. Durable kernel events and ephemeral generation-fenced resource streams remain separate. `StreamFrame` keeps terminal/browser payloads outside durable domain events while preserving ordered snapshot/delta/resync. Unknown future discriminants decode only as bounded `GenericExtensionFrame { type_id, schema_version, redacted_payload }`; the domain command/event decoder remains closed and never applies them as facts.
- [ ] **Step 7: Commit golden fixtures** under `tests/fixtures/protocol/v1/`; changing them requires an intentional protocol-major/minor decision and review.
- [ ] **Step 8: Run** `cargo test --test protocol_contract -- --nocapture`; commit as `feat(protocol): add negotiated bounded chunked wire contract`.

### Task 1.7: Preserve only supported configuration contracts

**Files:** `src/config/{mod,model,project_store,remote_store}.rs`, `scripts/native-next/PhaseGate.ps1`, `tests/{configuration_contract,development_isolation}.rs`

- [ ] **Step 0: Register one exact guarded recipe** `phase-01-configuration` as `cargo test --test configuration_contract -- --nocapture`; lock its vector and the unchanged no-arguments public surface in `tests/development_isolation.rs` before implementation.
- [ ] **Step 1: Write failing tests** that load current `config.json` and `remote.json` fixtures, round-trip unknown supported fields, create recoverable backups, atomically replace files, recover from an interrupted/corrupt replacement, retain the existing field currently named invite code as the long-lived device pairing code, and prove no code path opens `session.json`.
- [ ] **Step 2: Run** `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-01-configuration-red -Recipe phase-01-configuration -LongRustRun` and retain the red result with production/process evidence.
- [ ] **Step 3: Move or re-export the stable project/folder/command/SSH types** from `src/models/config.rs` into `src/config/model.rs` without changing their serialized field names. Keep a single source of truth; temporary re-exports go in the Phase 0 deletion ledger.
- [ ] **Step 4: Implement atomic stores** with validated content, recoverable same-directory backups, same-directory temporary files, flush, replace, and permission preservation. A parse/write/interruption failure leaves or restores the last valid original and surfaces the exact path/error.
- [ ] **Step 5: Preserve `remote.json` device records, revocations, pairing token, and manually rotatable long-lived device pairing code** across host/UI upgrades. Reading configuration must not rotate secrets. This credential is not reused as a later Task invitation.
- [ ] **Step 6: Add a test-only file-open observer** and assert the new config facade never requests `session.json`; do not write an importer or migration shim.
- [ ] **Step 7: Run** the same `phase-01-configuration` recipe green; commit as `refactor(config): preserve supported durable contracts`.

### Task 1.8: Integrate the headless kernel boundary

**Files:** `src/kernel/{mod,command_bus,outbox,runtime}.rs`, `src/lib.rs`, `scripts/native-next/PhaseGate.ps1`, `tests/{kernel_store,operation_lifecycle,development_isolation}.rs`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Add an integration test** that creates a temporary profile, creates two tasks through `CommandBus`, retries one command, observes the same OperationId, settles one side effect, streams ordered events, pages snapshots, begins closing one task, reopens the store, queries both operation states without creating operations, and obtains the identical projection.
- [ ] **Step 2: Run** `cargo test --test kernel_store headless_kernel_round_trip -- --nocapture` and confirm the integration is red before wiring the public boundary.
- [ ] **Step 3: Expose only** `CommandBus`, `KernelStore`, `RuntimeRegistry`, domain envelopes, and read APIs from `kernel::mod`; prevent UI code from reaching SQLite internals.
- [ ] **Step 4: Define the outbox claim/ack/recovery contract** with leases, dispatch-start timestamps, stable effect/idempotency identity, and the three replay policies. The phase does not send network traffic; tests prove retry-safe redelivery, reconcile-before-retry, and explicit uncertainty for ambiguous non-idempotent work. Never describe the generic outbox as exactly-once.
- [ ] **Step 5: Mark every temporary old-module re-export** with its exact deletion criterion in `docs/replacement-deletion-ledger.md`.
- [ ] **Step 6: Extend the closed recipe table.** Add exact no-argument recipes to `scripts/native-next/PhaseGate.ps1` and lock them in `tests/development_isolation.rs`: `phase-01-tests` runs `cargo test --test domain_identity --test task_state --test kernel_store --test operation_lifecycle --test protocol_contract --test configuration_contract -- --nocapture`; `phase-01-clippy` runs `cargo clippy --lib --tests -- -D warnings`. Do not add command or argument parameters. Reuse the existing `cargo-fmt-check` recipe for formatting.
- [ ] **Step 7: Run** the integration test and the named recipes through the isolation gate: `phase-01-tests`, `cargo-fmt-check`, and `phase-01-clippy`.
- [ ] **Step 8: Commit** as `feat(kernel): expose durable headless kernel boundary`.

## Phase 1 verification gate

- [ ] Capture the production baseline with Phase 0 tooling.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-01-tests -Recipe phase-01-tests -LongRustRun`.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-01-fmt -Recipe cargo-fmt-check`.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-01-clippy -Recipe phase-01-clippy -LongRustRun`.
- [ ] Inspect `kernel_store` with `PRAGMA integrity_check`, replay all facts into shadow projections, and compare row-for-row.
- [ ] Search for forbidden dependencies with `rg -n "gpui|wry|WebView|portable_pty|std::process::Command" src/domain src/kernel src/protocol` and require no runtime/UI matches.
- [ ] Search for accidental session migration with `rg -n "session\.json|codex_rollout" src/config src/domain src/kernel src/protocol tests`; only the negative contract test may mention `session.json`, and `codex_rollout` must not appear.
- [ ] Compare production invariants and confirm the installed PID/start time are unchanged.
- [ ] Confirm no Cargo, rustc, test harness, development host, provider, or browser helper remains.
- [ ] Review the complete Phase 1 diff and update the deletion ledger before beginning Phase 2.

## Phase 1 exit criteria

- A fresh profile creates a valid SQLite v1 store without any UI or runtime process.
- Every accepted mutation is atomic, idempotent, revision checked, event-backed, and correlated to a separately queryable outcome; ambiguous external delivery is visible `Uncertain` and never automatically duplicated.
- Paged snapshots plus ordered events deterministically reconstruct every durable read model.
- Protocol major/minor, negotiated physical/reassembled/page/chunk bounds, unknown-extension behavior, and capability behavior are fixed by golden frames.
- Existing project configuration and stable remote pairing survive direct load/save; legacy session state is never opened.
- Production storage and the installed DevManager remain byte/process-identical across the gate.
