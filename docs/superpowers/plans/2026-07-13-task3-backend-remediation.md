# Task 3 Backend Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the seven independently reviewed Task 3 backend correctness and resource-bound gaps without changing the browser UI contract.

**Architecture:** Keep the host as the single authority. Centralize request admission and response waiting behind bounded queues, extend the existing keyed input executor with one global retained-payload budget, and carry a small web-authority token to the post-staging composer boundary. Preserve the existing single-value `replacesSequence` browser contract by retaining the bounded sequence of replacement events needed to replay a dedupe chain.

**Tech Stack:** Rust 2021, std synchronization/channels/threads, Tokio/Axum WebSocket, Serde, inline Rust unit tests.

## Global Constraints

- Write and run a failing regression before every production behavior change.
- Keep browser wire changes additive and camelCase; do not edit TypeScript/PWA files.
- Never persist browser semantic history or authority state.
- Keep external PTY and filesystem callbacks outside global protocol locks.
- Bound every new queue by both an item count and, for retained input, bytes.
- Run focused tests after each red/green cycle and the complete Rust library suite once after all fixes.

---

### Task 1: Clear expired web controller ownership

**Files:**
- Modify/test: `src/remote/web/bridge.rs`

- [ ] Add `expired_request_authorization_clears_controller_and_allows_reacquire`, acquiring a generation lease, authorizing a Request after expiry, and asserting a second connection can acquire automatically.
- [ ] Run the focused test and confirm the stranded `controller_client_id` failure.
- [ ] In `with_web_mutation_authority`, capture the lease before and after `authorize`, call `clear_controller_after_lease_removal`, and retain the existing lease-state broadcast.
- [ ] Run the focused test and adjacent lease tests.

### Task 2: Make chained semantic replacement replay-safe

**Files:**
- Modify/test: `src/remote/presentation.rs`

- [ ] Add `chained_deduplication_replay_deletes_every_observable_predecessor`: push three events under one dedupe key, replay after the first sequence, apply each single `replaces_sequence`, and assert only the final sequence remains.
- [ ] Run the focused test and confirm the replay currently contains only the last event replacing the second.
- [ ] When replacing an event that itself replaces an earlier sequence, retain that bounded predecessor in the journal but clear its active dedupe index; continue removing the first non-replacement event. Existing event/byte retention remains the hard bound.
- [ ] Run all presentation tests.

### Task 3: Bound resume projection capture attempts and deferred work

**Files:**
- Modify/test: `src/remote/web/bridge.rs`

- [ ] Add `resume_projection_generation_retry_is_bounded`, using a capture closure that changes the generation every attempt and asserting exactly `MAX_RESUME_CAPTURE_ATTEMPTS` captures and a returned fallback.
- [ ] Run the focused test and confirm the helper/limit is absent.
- [ ] Extract a bounded generation-capture helper that accepts only an even, unchanged generation and otherwise returns its final internally coherent capture after the fixed attempt count.
- [ ] Move `SemanticReplayCapture::into_replay`, sorting, and mobile byte-cap serialization after bounded capture selection so discarded attempts only clone bounded journal pointers/metadata.
- [ ] Run the capture/publication/replay bridge tests.

### Task 4: Bound pending host requests and response waiters

**Files:**
- Create/test: `src/remote/web/request_executor.rs`
- Modify: `src/remote/web/mod.rs`
- Modify/test: `src/remote/mod.rs`
- Modify/test: `src/remote/web/bridge.rs`

- [ ] Add a request-executor test proving one blocked worker plus one queued waiter rejects a third job without spawning another worker.
- [ ] Run it and confirm the executor is missing.
- [ ] Implement a fixed worker pool over `sync_channel`, with nonblocking admission and graceful channel shutdown.
- [ ] Add a failing bridge regression that fills `MAX_PENDING_REMOTE_REQUESTS`, sends a Request, receives an immediate capacity error, and observes no queue growth.
- [ ] Add `try_enqueue_pending_request` as the only queue mutation path and use it for web and native Action/Request frames.
- [ ] Reserve response-waiter capacity before enqueuing web Requests; gate the reserved waiter until authority and host-queue admission succeed, and fail immediately if either bounded layer is full.
- [ ] Run request executor, bridge request, and remote transport tests.

### Task 5: Add a global retained input budget

**Files:**
- Modify/test: `src/remote/web/input_executor.rs`
- Modify/test: `src/remote/web/bridge.rs`

- [ ] Add `global_budget_counts_running_and_queued_payloads_across_keys`, proving a blocked payload plus another key exhausts item/byte admission and that completion releases both counters.
- [ ] Run it and confirm current per-key admission accepts the excess job.
- [ ] Wrap queued jobs in a drop-based global item/byte reservation; reject admission above `MAX_WEB_INPUT_ITEMS` or `MAX_WEB_INPUT_BYTES` while preserving per-key FIFO/queue limits.
- [ ] Pass exact retained text/byte/attachment sizes from raw input, paste, interrupt, resize, and composer dispatch sites.
- [ ] Run all input executor and bridge queue tests.

### Task 6: Revalidate composer authority after attachment staging

**Files:**
- Modify/test: `src/remote/mod.rs`
- Modify/test: `src/remote/web/bridge.rs`
- Modify/test: `src/remote/web/image_paste.rs`
- Modify: `src/app/mod.rs`

- [ ] Add an image-paste regression that stages valid files, makes the boundary authority callback return false, asserts zero PTY writes, and asserts every staged file was rolled back.
- [ ] Run it and confirm no boundary callback exists.
- [ ] Add a small serializable `RemoteWebMutationAuthority` token to `ComposerBatch`; expose a host validation method that checks runtime, paired active connection, exact generation, lease, and controller.
- [ ] Call that validation only after all attachments stage and immediately before the single PTY write; return the internal authority-changed sentinel and roll back staged files when stale.
- [ ] Map that sentinel to transient `StaleGeneration`, clear the in-flight mutation record, and avoid storing a terminal rejection.
- [ ] Add/run a bridge regression that hands authority to native during the callback and can retry the same mutation without a stale PTY write.

### Task 7: Project interactive server-shell mode

**Files:**
- Modify/test: `src/remote/web/dto.rs`

- [ ] Add a DTO projection test setting `SessionRuntimeState::interactive_shell = true` and expecting JSON `interactiveShell: true`, while a default session emits `false`.
- [ ] Run it and confirm the field is absent.
- [ ] Add `interactive_shell: bool` to `WebSessionSummary` and populate it directly from the redacted runtime state.
- [ ] Run DTO and bridge projection tests.

### Final verification and commit

- [ ] Run targeted rustfmt checks and `git diff --check`.
- [ ] Run `cargo test --lib` once after the final production change.
- [ ] Run `cargo check --lib`.
- [ ] Review the diff for frontend overlap, unbounded state, lock-held callbacks, and test-only leakage.
- [ ] Commit all Task 3 backend remediation with one coherent commit and report its SHA plus exact verification results.
