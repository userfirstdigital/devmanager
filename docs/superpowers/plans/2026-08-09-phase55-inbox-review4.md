# Task 5.5 Inbox Review 4 Correction Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the isolated native-next Task Cockpit consume one real caller-driven HostClient subscription, recover safely, and keep inbox search/projection bounded and truthful.

**Architecture:** A native-next-only bootstrap owns an explicit `InboxHostController` and hands its shared subscription to an `InboxRuntime`/shell model; pump methods are called from a controller/task lane and never from paint/input. Resync fences the projection until a fresh snapshot+replay succeeds. Client preferences remain a dedicated isolated file. Search postings retain compact task IDs with bounded candidate/result work and deterministic projection ordering.

**Tech Stack:** Rust, Tokio, GPUI projection contracts, existing HostClient/ClientSubscription protocol, isolated tempfile fixtures.

## Global Constraints

- Do not modify or attach `app::NativeShell`, legacy `SessionState`, default profile resolution, or production `session.json`.
- Do not start the production host from preview; preview remains headless/fixture-only.
- Use explicit `HostClientConfig` and `InboxPreferenceStore` paths in tests.
- Keep host I/O off paint/input; the shell receives only bounded projection state.
- Preserve revision/focus fences, accessibility, virtual caps, deterministic occurred-time order, and truthful search totals.

### Task 1: Native-next bootstrap and visible shell consumption

**Files:** `src/ui/task_cockpit/bootstrap.rs`, `src/ui/task_cockpit/mod.rs`, `src/ui/task_cockpit/inbox.rs`, `tests/ui_projection.rs`

- [x] Add a failing integration test that constructs an explicit fixture controller/subscription, bootstraps the native-next shell, synchronizes through the controller, and asserts a non-empty visible render model without touching legacy app/session state.
- [x] Run the focused test and record the missing bootstrap/empty render failure.
- [x] Implement a native-next-only bootstrap with explicit controller injection, caller-driven `synchronize`/`receive_one`, cursor restore/persist, and a shell model that consumes `InboxRuntime::render_model`.
- [x] Run the focused test green and verify no legacy source path is referenced.

### Task 2: Reconnect, resync fencing, and replay overflow

**Files:** `src/client/inbox_controller.rs`, `src/client/subscription.rs`, `src/ui/task_cockpit/inbox.rs`, `tests/ui_projection.rs`, `tests/host_lifecycle.rs`

- [x] Add failing tests for reconnect recreating the subscription before synchronization, stale projection visibility while `NeedsResync`, authoritative projection replacement after resync, and typed overflow failure.
- [x] Implement generation reset/recreation, a typed `ReplayOverflow` error, and projection invalidation until successful synchronization; never silently evict replay events.
- [x] Run subscription/controller/projection tests green.

### Task 3: Bounded preference reads

**Files:** `src/client/preferences.rs`, tests in the same module or `tests/ui_projection.rs`

- [x] Add a failing oversized-file test that proves the reader checks metadata and rejects before allocating/decoding.
- [x] Implement bounded metadata/read handling with the existing atomic write contract and version checks.
- [x] Run preference tests green.

### Task 4: Compact bounded search index

**Files:** `src/client/model.rs`, `tests/ui_projection.rs`

- [x] Add failing 100k tests for one-character/adversarial queries, bounded result work, truthful totals, deterministic occurred-time order, and no title-cloned posting keys.
- [x] Replace repeated `TaskOrderKey` postings with compact task-ID postings and explicit caps/fast paths; keep event updates incremental and search results bounded.
- [x] Run 100k projection/search tests green, then run the focused regression suite.

### Task 5: Final verification

- [x] Run format, focused UI/subscription/controller/preferences/model tests, host lifecycle/replay/restart tests, `cargo check --lib -j1`, and the serial library suite with the required fresh target and no external profile.
- [ ] Review the complete diff, verify no production process/profile/session access, commit the correction, and leave the worktree clean.
