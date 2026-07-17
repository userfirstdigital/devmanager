# Browser Replay Domain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the checkpoint-7 platform-neutral replay compiler, safe status state machine, exact workspace/instance fencing, bounded terminal cleanup, and one replay-lifetime cancellation lease without executing browser work.

**Architecture:** One new `src/browser/replay.rs` module contains a pure compiler and a cloneable mutex-backed coordinator. Value-bearing inputs/plans/leases are non-Debug and non-Serialize; only fixed errors and value-free projections cross diagnostic or serialization boundaries.

**Tech Stack:** Rust, serde, static_assertions, existing `BrowserRecipeV1` and `BrowserWorkspaceKey` domain types.

## Global Constraints

- Approved base is `0f35ff6552faadf7fa0226d4e59359030848562c`; checkpoint 7 only.
- Do not execute or call host/controller/queue/approval/journal/filesystem/UI/MCP/secret/repair code.
- Public File values are bounded nonblank NUL/control-free opaque candidates; do not normalize or inspect them.
- One immutable cancellation authority is minted once per replay and shared by every lease clone across every status/step gap.
- No input literal, file path, recipe value, or arbitrary message may enter status, error, Debug, or Serialize output.

---

### Task 1: Compiler and value-safe immutable plan

**Files:**
- Create: `tests/browser_replay.rs`
- Create: `src/browser/replay.rs`
- Modify: `src/browser/mod.rs`
- Modify: `src/browser/recipes.rs`

**Interfaces:**
- Consumes: `BrowserRecipeV1`, `BrowserRecipeInputKind`, `BrowserRecipeValue`, `BrowserRecipeStep`, `BrowserRecipeViewport`.
- Produces: `BrowserReplayPublicInput::new`, `compile_browser_replay`, `BrowserReplayPlan`, `BrowserReplayError`.

- [ ] **Step 1: Write the failing compiler tests**

```rust
let plan = compile_browser_replay(
    &recipe_with_text_url_file_and_secret(),
    vec![
        BrowserReplayPublicInput::new("query", BrowserRecipeInputKind::Text, "rust"),
        BrowserReplayPublicInput::new("upload", BrowserRecipeInputKind::File, "fixtures/a.txt"),
    ],
)?;
assert_eq!(plan.resolve_input("query"), Some("rust"));
assert_eq!(plan.resolve_input("destination"), Some("https://example.test/default"));
assert_eq!(plan.unresolved_secret_input_names(), &["password"]);
assert_eq!(plan.steps().iter().map(|step| step.id.as_str()).collect::<Vec<_>>(), vec!["type", "navigate", "upload"]);
```

Add separate assertions for invalid recipe, duplicate/unknown/missing input, kind mismatch, every public Secret attempt, credential-like/oversized/NUL Text, unsafe/oversized URL, and blank/control/oversized File candidates. Add compile assertions that public inputs and plans implement neither `Debug` nor `serde::Serialize` and are `Send + Sync`.

- [ ] **Step 2: Run the compiler tests and witness RED**

Run: `cargo test --locked --test browser_replay replay_compiler -- --test-threads=1`

Expected: unresolved replay imports/types because checkpoint 7 does not exist.

- [ ] **Step 3: Implement the minimal compiler**

```rust
pub struct BrowserReplayPublicInput {
    name: String,
    kind: BrowserRecipeInputKind,
    value: String,
}

pub struct BrowserReplayPlan {
    recipe_id: String,
    start_url: String,
    viewport: BrowserRecipeViewport,
    steps: Vec<BrowserRecipeStep>,
    bindings: HashMap<String, ReplayBoundValue>,
    unresolved_secret_inputs: Vec<String>,
}

pub fn compile_browser_replay(
    recipe: &BrowserRecipeV1,
    public_inputs: Vec<BrowserReplayPublicInput>,
) -> Result<BrowserReplayPlan, BrowserReplayError>;
```

Validate the recipe first; cap 64 inputs, 256 steps, 128-byte control-free names, 64-KiB Text, 8-KiB URL, and 32-KiB File. Refactor the existing safe URL validator to `pub(crate)` and map every underlying validation failure to a fixed `BrowserReplayError` variant. Apply only Text/URL defaults. Never accept Secret values.

- [ ] **Step 4: Run compiler tests GREEN and refactor without changing behavior**

Run: `cargo test --locked --test browser_replay replay_compiler -- --test-threads=1`

Expected: all compiler-prefixed tests pass with no value-bearing diagnostics.

---

### Task 2: Exact statuses, transitions, fencing, and bounded cleanup

**Files:**
- Modify: `tests/browser_replay.rs`
- Modify: `src/browser/replay.rs`

**Interfaces:**
- Consumes: `BrowserReplayPlan`, `BrowserWorkspaceKey`.
- Produces: `BrowserReplayStatus`, `BrowserReplayFailureCode`, `BrowserReplayInstance`, `BrowserReplayProjection`, `BrowserReplayCoordinator`.

- [ ] **Step 1: Write failing state tests**

```rust
let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
let started = coordinator.start(owner.clone(), plan_without_secrets())?;
assert_eq!(started.projection.status, BrowserReplayStatus::Pending);
assert_eq!(coordinator.begin(&started.instance)?.status, BrowserReplayStatus::Running);
assert_eq!(coordinator.advance_step(&started.instance, 0)?.current_step_index, 1);
assert_eq!(coordinator.pause_locator_repair(&started.instance)?.status, BrowserReplayStatus::PausedLocatorRepair);
assert_eq!(coordinator.resume_locator_repair(&started.instance)?.status, BrowserReplayStatus::Running);
```

Cover all seven exact statuses, early completion rejection, exact step ordering, one active replay per workspace, different-workspace isolation, explicit replacement, stale/late calls, typed Failed code, terminal immutability, and eviction of the oldest terminal projection at capacity two. Serialize and Debug projections/errors containing sentinel-backed plans and assert that no literal/path/sentinel appears.

- [ ] **Step 2: Run state tests and witness RED**

Run: `cargo test --locked --test browser_replay replay_state -- --test-threads=1`

Expected: unresolved coordinator/status interfaces.

- [ ] **Step 3: Implement the minimal coordinator state machine**

```rust
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum BrowserReplayStatus {
    Pending,
    Running,
    NeedsUserSecret,
    PausedLocatorRepair,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone)]
pub struct BrowserReplayCoordinator {
    inner: Arc<Mutex<BrowserReplayCoordinatorState>>,
}
```

Store one active value-bearing plan per workspace and only value-free terminal projections in a bounded deque. Enforce exact instance equality on every mutation. Remove the plan at terminal transition. Implement `begin`, checked `advance_step`, pause/resume, `complete`, typed `fail`, `cancel`, `replace`, `status`, and workspace interruption with only the approved legal transitions. Keep `secrets_ready` `pub(crate)` and value-free.

- [ ] **Step 4: Run state tests GREEN**

Run: `cargo test --locked --test browser_replay replay_state -- --test-threads=1`

Expected: all state-prefixed tests pass.

---

### Task 3: One replay-lifetime cancellation authority

**Files:**
- Modify: `tests/browser_replay.rs`
- Modify: `src/browser/replay.rs`

**Interfaces:**
- Consumes: coordinator lifecycle methods.
- Produces: `BrowserReplayCancellationLease::authority_id`, `same_authority`, and `is_cancelled`.

- [ ] **Step 1: Write failing lease tests**

```rust
let started = coordinator.start(owner.clone(), plan_without_secrets())?;
let clone = started.lease.clone();
coordinator.begin(&started.instance)?;
coordinator.advance_step(&started.instance, 0)?;
assert!(started.lease.same_authority(&clone));
assert!(!clone.is_cancelled());
coordinator.interrupt_workspace(&owner);
assert!(started.lease.is_cancelled());
assert!(clone.is_cancelled());
```

Repeat across Pending, NeedsUserSecret, Running step gaps, and PausedLocatorRepair. Prove replacement cancels the old authority, the new authority differs, late completion is rejected, and neither transitions nor status reads mint/rearm an authority. Assert lease is `Clone + Send + Sync` and neither `Debug` nor `Serialize`.

- [ ] **Step 2: Run lease tests and witness RED**

Run: `cargo test --locked --test browser_replay replay_cancellation -- --test-threads=1`

Expected: unresolved lease accessors and cancellation behavior.

- [ ] **Step 3: Implement the shared immutable authority**

```rust
struct BrowserReplayCancellationAuthority {
    id: u64,
    cancelled: AtomicBool,
}

#[derive(Clone)]
pub struct BrowserReplayCancellationLease {
    authority: Arc<BrowserReplayCancellationAuthority>,
}
```

Mint exactly once in start/replace, store a clone in active state, and set the shared atomic only on cancel/replacement/workspace interruption. Never replace or reset it at begin, advance, status, secret pause, or locator pause/resume.

- [ ] **Step 4: Run lease and full focused tests GREEN**

Run: `cargo test --locked --test browser_replay -- --test-threads=1`

Expected: every replay test passes.

---

### Task 4: Scope audit, documentation, and final verification

**Files:**
- Modify: `.superpowers/sdd/browser-task-5c-checkpoints.md`
- Modify: `.superpowers/sdd/browser-task-5c-report.md`
- Modify: `.superpowers/sdd/progress.md`

**Interfaces:**
- Consumes: completed checkpoint-7 domain and RED/GREEN evidence.
- Produces: immutable checkpoint report and review package.

- [ ] **Step 1: Add a scope/source regression and run focused tests**

Assert `replay.rs` contains no filesystem, host, controller, command, queue, approval, journal, MCP, UI, zeroizing secret store, or repair payload integration. Then run the full replay target.

- [ ] **Step 2: Update checkpoint documentation and self-review the exact base diff**

Record the compiler, safe projections, transition table, cancellation authority, bounds, tests, and explicit checkpoint-8-through-12 exclusions. Inspect `git diff 0f35ff6 -- src/browser tests/browser_replay.rs` for value leaks and scope drift.

- [ ] **Step 3: Run fresh completion gates**

Run:

```text
cargo test --locked --test browser_replay -- --test-threads=1
cargo test --locked browser -- --test-threads=1
cargo check --locked --all-targets
cargo build --locked
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits zero.

- [ ] **Step 4: Commit and package the immutable range**

Commit checkpoint 7, generate `.superpowers/sdd/review-0f35ff6..<head>.diff`, and report exact base/head, stable patch ID, SHA-256, and clean worktree.
