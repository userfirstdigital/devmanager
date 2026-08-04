# Phase 1: Domain, Store, and Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish the durable, presentation-independent task model, SQLite event store, idempotent command path, snapshots, and versioned wire protocol that every later host and client uses.

**Architecture:** Domain facts use typed UUIDv7 identifiers and pure deterministic transitions. Each accepted command commits its events, read projections, command receipt, and Connect outbox records in one SQLite transaction. Clients exchange bounded, length-prefixed MessagePack envelopes and recover from snapshots plus ordered events; neither GPUI nor web types are allowed in this layer.

**Tech Stack:** Rust 1.94.0, serde, rmp-serde, rusqlite 0.40.1 with bundled SQLite, uuid 1.24.0 with v7/serde, tempfile.

## Global Constraints

- This phase may not launch PTYs, providers, browsers, services, or UI windows.
- `TaskId`, `AgentSessionId`, `ArtifactId`, `ResourceId`, `ClientId`, `CommandId`, and `EventId` are distinct types, never strings or interchangeable UUID aliases.
- Facts are append-only. Projections are disposable and reproducible from facts.
- A retry with the same `CommandId` returns the stored receipt without appending another event or outbox item.
- One accepted command is one SQLite transaction: events, projections, receipt, and outbox either all commit or none commit.
- Optimistic concurrency uses `expected_task_revision`; a mismatch is a typed rejection, not a retry or silent overwrite.
- `config.json` and `remote.json` remain direct supported contracts. The new kernel must never read or infer state from `session.json`.
- Wire decoding is bounded before allocation. Unknown optional capabilities are ignored; incompatible protocol majors fail visibly.
- Use the Phase 0 isolated worktree, profile, target directory, evidence wrapper, and production guard for every command.

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
- Create: `tests/configuration_contract.rs`

### Task 1.1: Add typed identifiers and immutable task facts

**Files:** `Cargo.toml`, `src/domain/{mod,id,task,agent,artifact,resource}.rs`, `src/lib.rs`, `tests/domain_identity.rs`

**Contracts:**

```rust
pub struct TaskId(Uuid);
pub struct AgentSessionId(Uuid);
pub struct ArtifactId(Uuid);
pub struct ResourceId(Uuid);
pub struct ClientId(Uuid);

pub struct TaskFacts {
    pub id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
    pub assignment: TaskAssignment,
    pub lifecycle: TaskLifecycle,
    pub revision: u64,
    pub created_at_ms: i64,
}
```

- [ ] **Step 1: Write the failing identity tests** in `tests/domain_identity.rs` for UUIDv7 generation, serde round-trip, display/parse round-trip, invalid-version rejection, and compile-time non-interchangeability using `static_assertions::assert_not_impl_any!`.
- [ ] **Step 2: Run** `pwsh scripts/native-next/Invoke-PhaseGate.ps1 -Name phase-01-domain-red -Command 'cargo test --test domain_identity -- --nocapture'` **and record** the unresolved `devmanager::domain` failure in the phase evidence directory.
- [ ] **Step 3: Add** `rusqlite = { version = "0.40.1", features = ["bundled"] }` and `uuid = { version = "1.24.0", features = ["v7", "serde"] }`; implement a private `define_id!` macro whose public types validate UUID version 7 when parsed from external text.
- [ ] **Step 4: Define** `EnvironmentId`, `ProjectId`, `TaskId`, `AgentSessionId`, `ArtifactId`, `ResourceId`, `TerminalId`, `BrowserContextId`, `ServiceId`, `ClientId`, `CommandId`, `RequestId`, `SubscriptionId`, and `EventId`; expose UUID bytes for persistence without exposing cross-type conversion.
- [ ] **Step 5: Define** `WorkspaceRef::{Main, Worktree { path, branch }, External { path }}`, `TaskLifecycle::{Open, Closing, Archived}`, `TaskAssignment::{LocalOwner, ExternalPrincipal { authority, subject }}`, and fact records for task, agent, artifact, and resource ownership. Validate titles, descriptions, principal references, and paths at construction boundaries.
- [ ] **Step 6: Run** `cargo test --test domain_identity -- --nocapture` and confirm all identity tests pass.
- [ ] **Step 7: Commit** with `git add Cargo.toml Cargo.lock src/domain src/lib.rs tests/domain_identity.rs && git commit -m "feat(kernel): define durable domain identities"`.

### Task 1.2: Define versioned commands, events, receipts, and pure task transitions

**Files:** `src/domain/{command,event,task,snapshot}.rs`, `tests/task_state.rs`

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
    Accepted { command_id: CommandId, task_revision: u64, event_ids: Vec<EventId> },
    Rejected { command_id: CommandId, code: RejectionCode, current_revision: Option<u64> },
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

- [ ] **Step 1: Write failing tests** named `create_task_emits_revision_one`, `rename_requires_expected_revision`, `closing_is_idempotent`, `archived_task_rejects_new_runtime`, `agent_and_resource_must_reference_same_task`, `visible_status_precedence_is_deterministic`, and `replay_derives_identical_snapshot`.
- [ ] **Step 2: Run** `cargo test --test task_state -- --nocapture` and retain the missing command/event types as the expected red result.
- [ ] **Step 3: Define the initial commands:** `CreateTask`, `RenameTask`, `SetTaskAttention`, `CloseTask`, `ReopenTask`, `RegisterAgentSession`, `SetPrimaryAgent`, `RegisterArtifact`, `RegisterResource`, and `ReleaseResource`. Each carries complete intent; commands may not contain UI indexes or labels as identity.
- [ ] **Step 4: Define matching facts** plus separate axes `TaskConnectivity`, `TaskAttention::{None, NeedsAnswer, NeedsApproval, Failed}`, `TaskActivity::{Idle, Working, Settling}`, `ReviewReadiness::{NotReady, Ready}`, and typed rejection codes: `NotFound`, `AlreadyExists`, `RevisionConflict`, `InvalidTransition`, `OwnershipConflict`, and `UnsupportedCapability`.
- [ ] **Step 5: Implement** `decide(snapshot, command) -> Result<Vec<Event>, RejectionCode>` and pure `apply(snapshot, event)`. Derive `VisibleTaskStatus` with precedence disconnected, failed, needs approval, needs answer, working/settling, ready for review, idle; never store it as a second mutable truth.
- [ ] **Step 6: Make event serialization explicit** with `{ event_type, schema_version, payload }`; add golden JSON fixtures so renaming a Rust enum cannot silently change the durable format.
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
  committed_sequence INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL
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
  event_sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
  destination_class TEXT NOT NULL,
  payload BLOB NOT NULL,
  state TEXT NOT NULL,
  available_at_ms INTEGER NOT NULL,
  leased_until_ms INTEGER,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error_class TEXT
);
```

- [ ] **Step 1: Add failing store tests** for an empty migration, WAL mode, foreign-key enforcement, schema-version rejection, projection rebuild, corrupt/truncated database, interrupted transaction, and integrity-check failure. Assert exact table/index names with `sqlite_schema`.
- [ ] **Step 2: Run** `cargo test --test kernel_store schema_ -- --nocapture` and observe the missing schema module.
- [ ] **Step 3: Implement** ordered migrations in `schema_migrations`; open file databases with WAL, `foreign_keys=ON`, a bounded busy timeout, and `synchronous=NORMAL`; use full durability for explicit checkpoint/export operations.
- [ ] **Step 4: Store identifiers** as fixed 16-byte blobs, timestamps as Unix milliseconds, event/receipt payloads as MessagePack, and human-queryable discriminants as stable text. Add indexes for task sequence, task revision, outbox delivery state, and active resources.
- [ ] **Step 5: Implement projectors** for `tasks`, `agent_sessions`, `artifacts`, and `resources`. Projection functions accept a transaction and one event and contain no clocks, random generation, filesystem calls, or network calls.
- [ ] **Step 6: Implement `rebuild_projections`** into shadow tables, compare deterministic rows, then swap inside one transaction; this is a repair/verification tool, not normal startup work.
- [ ] **Step 7: Run** `cargo test --test kernel_store schema_ -- --nocapture`; commit as `feat(kernel): add sqlite event schema and projections`.

### Task 1.4: Make command execution atomic and idempotent

**Files:** `src/kernel/{store,command_bus,outbox}.rs`, `tests/kernel_store.rs`

**Contract:** `KernelStore::execute(CommandEnvelope) -> CommandReceipt` performs lookup, revision validation, decision, append, projection, receipt, and outbox insert in one immediate transaction.

- [ ] **Step 1: Add failing tests** `accepted_command_commits_all_records`, `projector_failure_rolls_back_every_record`, `duplicate_command_returns_original_receipt`, `revision_conflict_persists_only_rejection_receipt`, `duplicate_rejected_command_stays_rejected`, and `concurrent_writers_accept_only_one_revision`.
- [ ] **Step 2: Run** `cargo test --test kernel_store command_ -- --nocapture` and save the red output.
- [ ] **Step 3: Implement `KernelStore`** with a single writer connection owned by the kernel executor and read-only snapshot connections. No caller receives a raw writable `rusqlite::Connection`.
- [ ] **Step 4: Start an immediate transaction**, return an existing receipt before decision when `command_id` is known, load the current projection, compare `expected_task_revision`, and decide. Persist a final rejection receipt without events/projection/outbox; the same CommandId can never become accepted on retry.
- [ ] **Step 5: For acceptance, allocate event sequence, apply projections, and insert one outbox row per committed event**, serialize the final receipt, and commit. Convert constraint, corruption, and busy failures into distinct `StoreError` variants; never claim acceptance before commit succeeds.
- [ ] **Step 6: Add a deterministic failpoint used only by tests** between event insertion and projection so the rollback test proves atomicity rather than merely checking the happy path.
- [ ] **Step 7: Run** `cargo test --test kernel_store command_ -- --nocapture`; commit as `feat(kernel): execute commands atomically`.

### Task 1.5: Add snapshots, ordered replay, and runtime generation fences

**Files:** `src/domain/snapshot.rs`, `src/kernel/{store,runtime}.rs`, `tests/kernel_store.rs`

- [ ] **Step 1: Write failing tests** for `snapshot_has_global_cursor`, `events_after_cursor_are_strictly_ordered`, `expired_cursor_requires_snapshot`, `runtime_generation_rejects_stale_completion`, and `archived_task_has_no_live_resources`.
- [ ] **Step 2: Run** `cargo test --test kernel_store replay_ -- --nocapture` and record the red result.
- [ ] **Step 3: Define** `KernelSnapshot { through_sequence, tasks, agent_sessions, artifacts, resources }` and `EventPage { after_sequence, through_sequence, events, more }`. Keep ephemeral terminal grids and screenshots outside the durable snapshot.
- [ ] **Step 4: Add retention metadata** and a typed `ReplayUnavailable { oldest_sequence, newest_sequence }`; a client must request a fresh snapshot instead of guessing across a gap.
- [ ] **Step 5: Implement `RuntimeRegistry`** keyed by typed resource ID with monotonically increasing generation. Every asynchronous completion carries `(resource_id, generation)` and stale completions are discarded.
- [ ] **Step 6: Add bounded store maintenance** for WAL checkpoints, expired receipt/outbox cleanup after the documented idempotency window, integrity checks, and optional event-retention snapshots. Canonical facts are never pruned without an explicit retention policy and a replay boundary.
- [ ] **Step 7: On recovery**, load durable resource recipes as `Recovering` facts, reconcile actual processes in later phases, and never convert stored metadata into a claim that a process is still alive.
- [ ] **Step 8: Run** the focused replay/runtime/maintenance tests and commit as `feat(kernel): add snapshots replay and generation fences`.

### Task 1.6: Define capability negotiation and bounded MessagePack framing

**Files:** `src/protocol/{mod,capabilities,envelope,frame}.rs`, `tests/protocol_contract.rs`

**Wire handshake:**

```rust
pub struct ClientHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub client_build: String,
    pub client_id: ClientId,
    pub requested: CapabilitySet,
}

pub enum ClientMessage {
    Hello(ClientHello),
    Command(CommandEnvelope),
    Subscribe(SubscriptionRequest),
    Unsubscribe { subscription_id: SubscriptionId },
    Ack { scope: AckScope, through_sequence: u64 },
}

pub enum ServerMessage {
    Hello(ServerHello),
    Receipt(CommandReceipt),
    Snapshot(KernelSnapshot),
    Events(EventPage),
    Stream(StreamFrame),
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
```

- [ ] **Step 1: Write failing protocol tests** for byte-for-byte command/snapshot/event/stream golden frames, fragmented reads, coalesced frames, zero-length rejection, a `8 * 1024 * 1024` control-frame limit, scoped stream/global resync, unknown minor capability tolerance, major-version rejection, and malformed MessagePack closure.
- [ ] **Step 2: Run** `cargo test --test protocol_contract -- --nocapture` and record the missing protocol failure.
- [ ] **Step 3: Define** protocol major/minor constants and named capabilities for snapshots, event replay, semantic conversation, terminal deltas, browser projection, Connect encryption, guests, and management metadata.
- [ ] **Step 4: Implement** an unsigned 32-bit big-endian length prefix. Validate length before allocation, read exactly one frame, and deserialize with depth/collection bounds; bulk screenshots/files use later chunk messages rather than increasing the control limit.
- [ ] **Step 5: Add request correlation and subscription semantics** for durable kernel events plus ephemeral generation-fenced resource streams. `StreamFrame` keeps terminal/browser payloads outside durable domain events while preserving ordered snapshot/delta/resync; preserve unknown event/capability fields only where forward compatibility is explicitly promised.
- [ ] **Step 6: Commit golden fixtures** under `tests/fixtures/protocol/v1/`; changing them requires an intentional protocol-major/minor decision and review.
- [ ] **Step 7: Run** `cargo test --test protocol_contract -- --nocapture`; commit as `feat(protocol): add versioned bounded wire contract`.

### Task 1.7: Preserve only supported configuration contracts

**Files:** `src/config/{mod,model,project_store,remote_store}.rs`, `tests/configuration_contract.rs`

- [ ] **Step 1: Write failing tests** that load current `config.json` and `remote.json` fixtures, round-trip unknown supported fields, create recoverable backups, atomically replace files, recover from an interrupted/corrupt replacement, retain the pairing token/invite code, and prove no code path opens `session.json`.
- [ ] **Step 2: Run** `cargo test --test configuration_contract -- --nocapture` and retain the red result.
- [ ] **Step 3: Move or re-export the stable project/folder/command/SSH types** from `src/models/config.rs` into `src/config/model.rs` without changing their serialized field names. Keep a single source of truth; temporary re-exports go in the Phase 0 deletion ledger.
- [ ] **Step 4: Implement atomic stores** with validated content, recoverable same-directory backups, same-directory temporary files, flush, replace, and permission preservation. A parse/write/interruption failure leaves or restores the last valid original and surfaces the exact path/error.
- [ ] **Step 5: Preserve `remote.json` device records, revocations, pairing token, and manually rotatable invite code** across host/UI upgrades. Reading configuration must not rotate secrets.
- [ ] **Step 6: Add a test-only file-open observer** and assert the new config facade never requests `session.json`; do not write an importer or migration shim.
- [ ] **Step 7: Run** `cargo test --test configuration_contract -- --nocapture`; commit as `refactor(config): preserve supported durable contracts`.

### Task 1.8: Integrate the headless kernel boundary

**Files:** `src/kernel/{mod,command_bus,outbox,runtime}.rs`, `src/lib.rs`, `tests/kernel_store.rs`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Add an integration test** that creates a temporary profile, creates two tasks through `CommandBus`, retries one command, streams ordered events, snapshots, closes one task, reopens the store, and obtains the identical projection.
- [ ] **Step 2: Run** `cargo test --test kernel_store headless_kernel_round_trip -- --nocapture` and confirm the integration is red before wiring the public boundary.
- [ ] **Step 3: Expose only** `CommandBus`, `KernelStore`, `RuntimeRegistry`, domain envelopes, and read APIs from `kernel::mod`; prevent UI code from reaching SQLite internals.
- [ ] **Step 4: Define the outbox claim/ack/retry contract** with leases and attempt timestamps. The phase does not send network traffic; tests simulate dispatch and prove crash-safe redelivery.
- [ ] **Step 5: Mark every temporary old-module re-export** with its exact deletion criterion in `docs/replacement-deletion-ledger.md`.
- [ ] **Step 6: Run** the integration test, `cargo test --test domain_identity --test task_state --test kernel_store --test protocol_contract --test configuration_contract -- --nocapture`, then `cargo fmt --all -- --check` and `cargo clippy --lib --tests -- -D warnings` through the isolation wrapper.
- [ ] **Step 7: Commit** as `feat(kernel): expose durable headless kernel boundary`.

## Phase 1 verification gate

- [ ] Capture the production baseline with Phase 0 tooling.
- [ ] Run `cargo test --test domain_identity --test task_state --test kernel_store --test protocol_contract --test configuration_contract -- --nocapture`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --lib --tests -- -D warnings`.
- [ ] Inspect `kernel_store` with `PRAGMA integrity_check`, replay all facts into shadow projections, and compare row-for-row.
- [ ] Search for forbidden dependencies with `rg -n "gpui|wry|WebView|portable_pty|std::process::Command" src/domain src/kernel src/protocol` and require no runtime/UI matches.
- [ ] Search for accidental session migration with `rg -n "session\.json|codex_rollout" src/config src/domain src/kernel src/protocol tests`; only the negative contract test may mention `session.json`, and `codex_rollout` must not appear.
- [ ] Compare production invariants and confirm the installed PID/start time are unchanged.
- [ ] Confirm no Cargo, rustc, test harness, development host, provider, or browser helper remains.
- [ ] Review the complete Phase 1 diff and update the deletion ledger before beginning Phase 2.

## Phase 1 exit criteria

- A fresh profile creates a valid SQLite v1 store without any UI or runtime process.
- Every accepted mutation is atomic, idempotent, revision checked, event-backed, and replayable.
- A snapshot plus ordered events deterministically reconstructs every durable read model.
- Protocol major/minor and capability behavior is fixed by golden frames.
- Existing project configuration and stable remote pairing survive direct load/save; legacy session state is never opened.
- Production storage and the installed DevManager remain byte/process-identical across the gate.
