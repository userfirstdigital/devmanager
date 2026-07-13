# Host Work and Lease Follow-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep actual host Git execution bounded after browser response timeouts and reduce expired writer leases before stale-generation authorization returns.

**Architecture:** Add a host-owned, nonblocking atomic RAII concurrency limiter to `RemoteHostInner`. The app must acquire a permit before detaching a Git task and move that permit into the background closure so only task completion releases capacity. Move writer-lease expiry ahead of generation comparison while preserving `Expired` for a caller whose generation matched immediately before expiry.

**Tech Stack:** Rust 2021, GPUI background executor, `Arc<AtomicUsize>`, inline unit tests.

## Global Constraints

- Write and run each regression before production changes.
- Keep the GPUI/UI thread nonblocking and reject excess Git work immediately.
- Do not tie the host-work permit to a browser response receiver or waiter lifetime.
- Do not modify frontend/PWA/provider code.

---

### Task 1: Bound actual host Git work

**Files:**
- Modify/test: `src/remote/mod.rs`
- Modify: `src/app/mod.rs`

**Interfaces:**
- Produces: `RemoteHostService::try_acquire_work_permit() -> Option<RemoteHostWorkPermit>`.
- Consumes: the permit is moved into the existing Git background closure and dropped only after the Git match finishes.

- [x] Add `host_work_permits_survive_response_timeouts_until_jobs_finish`, using two blocked jobs, timed-out response receivers, and a third rejected acquisition.
- [x] Run `cargo test host_work_permits_survive_response_timeouts_until_jobs_finish --lib` and confirm the limiter API is absent.
- [x] Implement a fixed-size host work limiter with an RAII permit and a default limit of eight.
- [x] Acquire before `cx.spawn`; immediately send `RemoteActionResult::error("Remote host Git work is busy. Retry shortly.")` when saturated.
- [x] Move the permit into the background Git closure so waiter timeout/disconnect cannot release it.
- [x] Run the focused test and remote/app unit tests.

### Task 2: Reduce expiry before stale-generation authorization

**Files:**
- Modify/test: `src/remote/web/lease.rs`
- Modify/test: `src/remote/web/bridge.rs`

**Interfaces:**
- `WriterLeaseManager::authorize` keeps its existing signature and returns `Expired` only when the supplied generation matched before expiration; other generations receive `StaleGeneration` with the post-invalidation generation.

- [x] Add a lease-manager test proving a stale generation after expiry clears `peek()` and advances generation.
- [x] Add a bridge test proving the same path clears `controller_client_id` and broadcasts ownerless writer status.
- [x] Run both focused tests and confirm the expired owner remains installed.
- [x] Check expiry before generation mismatch, invalidate once, and classify the error from the pre-expiry generation match.
- [x] Run focused lease and bridge tests.

### Final verification and commit

- [x] Run targeted `rustfmt`, `git diff --check`, `cargo test --lib`, and `cargo check --lib`.
- [x] Stage only the plan and backend Rust files, commit, and report the SHA and exact results.
