Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Verify real user and operator journeys in owned environments

Phase 08 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The goal is checked where its users and operators can observe it. Browser, service logs and native interactions provide complementary evidence. Disposable test setup can proceed within the user’s authority, while shared or production targets remain protected. Checks links every failed step to retained observations even when diagnosis is incomplete.

## Preconditions

- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) exposes **CONTRACT-G07**. Run `cargo test --locked --test goal_repairs -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G08 with the existing browser gateway, replay/recording, Secret flow and process/service ownership. Qualify browser access from the actual worker environment; tool registration alone is insufficient. Resolve visible plan descriptions to actual routes/control labels and pin them in TEST-PLAN with current source version. A DevManager native acceptance journey uses actual GPUI Computer Use, not the browser target gallery.
2. Start every owned service with logs captured, retain a settled startup/idle baseline and each action’s start/end log offsets. Record console warnings/errors, failed/hanging requests, status/body shape, timings and appended service WARN/ERROR output even when the browser looks successful. Exercise user and operator personas plus empty/loading/error/partial states. A symptom with unknown cause remains a finding with the failed outcome and evidence; diagnosis can refine it later.
3. Provision, migrate, seed and reset only a named run-owned disposable environment permitted by current project/user policy. Bind a setup receipt to the actual endpoint/database identity and ownership marker, verify that identity immediately before mutation, and retain cleanup or explicit preserved-state disposition. Never infer disposable ownership from a hostname substring. Missing credentials use secure handoff; do not place literals in docs or context.
4. Route feature-caused findings into the goal audit with stable T- IDs and existing problems into the project’s merged tracker, preserving DONE recurrence history. Repeated runs update the same item, retain baselines and use independent final confirmation. Native Checks and Browser expose pinned journeys, hidden server warnings, unobservable requirements and exact missing tooling.

Implement the test-only `scripts/goal-acceptance.py` seed/start/status/adopt-native/stop/assert-clean commands and the exact fixture HTTP/ownership/log manifest contracts in TEST-PLAN's Setup and Non-visual surfaces sections. Use Python's standard library, owned fixture directories and real served user/operator pages. The feature/empty-project agreements and their exact checks are fixture inputs, not new Command requirements. Add these command/cleanup cases to `tests/goal_journeys.rs`; the utility must refuse foreign roots and reused PIDs. This makes the actual browser/log/setup journey directly runnable in this slice.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| E01 | A user journey needs browser access, but no eligible browser tool is available | E2E begins | The journey is NOT VERIFIED with the exact missing capability; a simulated account of clicking does not pass. |
| E02 | An export action returns success while its service logs a new exception | E2E executes the action | The tester records the visible success and hidden error with the relevant log interval, investigates cause, and adds a finding. |
| E03 | A journey fails, but its root cause is not yet known | The tester finishes its observation | The failure remains recorded with diagnosis incomplete and follow-up work; it is not omitted for lacking a code citation. |
| E07 | The target project has no custom budget and the recorded warm data response p95 is 800 ms | E2E checks the journey and its required state variants | It records the 500 ms budget miss and evidence, investigates the cause, and also verifies specified empty/error/partial-failure states rather than accepting only the successful populated view. |
| C03 | E2E needs a migration and the lead can create a permitted disposable test database owned by the run | Setup reaches that dependency | The lead provisions, migrates, seeds, and tests that database without another confirmation, and retains production migration files/run-sheet without applying them to production. |
| C05 | A connection labelled “test” resolves to a pre-existing shared/production resource, or an explicit restriction forbids using that environment | A migration or reset is requested | Ownership validation refuses the action; the lead provisions a permitted owned alternative when possible or reports the exact access blocker. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_journeys.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P08-E01** — `cargo test --locked --test goal_journeys p08_e01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p08_e01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P08-E02** — `cargo test --locked --test goal_journeys p08_e02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p08_e02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P08-E03** — `cargo test --locked --test goal_journeys p08_e03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p08_e03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P08-E07** — `cargo test --locked --test goal_journeys p08_e07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p08_e07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P08-C03** — `cargo test --locked --test goal_journeys p08_c03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p08_c03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P08-C05** — `cargo test --locked --test goal_journeys p08_c05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p08_c05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P08-J1 — Invisible server failure:** Execute a real owned fixture app journey where the UI succeeds and the server logs an action-correlated warning/error. Retain startup baseline plus exact appended offsets and a finding visible from native Checks. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P08-J2 — Environment authority:** Set up/migrate/seed/reset a proven run-owned disposable database, then attempt the same command with a changed shared/production target. The first has current setup/cleanup receipts; the second refuses before mutation and preserves the evidence. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P08-J3 — Tooling and real states:** Run a browser journey and real native provider-input journey with empty/loading/error/partial failure. Remove browser access and repeat: the requirement is Not verified with a precise prerequisite, never simulated. An unknown root cause remains a failed journey finding. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P08-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-PLAN`, `REF-TESTS`, `REF-CHECK-COVERAGE`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P08-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

### Shared verification and landing

Run commands only from the active isolated DevManager worktree with the Context Pack’s owned target/process preparation. These are checks to run during implementation, **not results of document generation**. Before the first Rust invocation print and validate `CARGO_TARGET_DIR`; restore the ignored WASM inputs, build the process-test helper and use one quiet Rust verification owner. A wrapper yield/timeout does not end Cargo ownership.

- `cargo fmt --all -- --check` — Rust formatting clean (this repository’s formatting/lint gate).
- `cargo check --locked --lib --bins --tests` — compiler/type gate succeeds in the isolated target.
- `cargo build --locked --bin devmanager-process-test-helper` — exact sibling helper exists before the complete suite.
- `cargo test --locked --lib -- --test-threads=1` — complete serial existing library suite; retain exact failing identities and completion summary.
- `cargo test --locked --tests -- --test-threads=1` — complete integration lane, including this phase’s new target.
- `cargo build --locked --bin devmanager --bin devmanager-host` — actual application/host build succeeds.

Run the phase’s focused checks while iterating and the shared global gates once at its required landing/integration boundary; reuse a still-current result across phases instead of rerunning global suites per scenario. Resolve introduced failures. Report exact pre-existing failures and incomplete environment runs; neither can certify a missing required behavior. If this phase changes web/Connect assets, also run `npm --prefix web test`, `npm --prefix web run typecheck`, and `npm --prefix web run build`. The web package has no lint script; do not invent one. Required Windows/Linux/native CI lanes in the Context Pack remain additional platform acceptance.


Each actual landing uses explicit scoped pathspecs and the current applicable review/commit policy. Commit only the phase/feature files owned by this work in every repo actually touched; this pack changes one app repository. Do not force a per-phase commit when the actual policy requires feature-level landing. Before ending, record built versus verified checks, current evidence and the exact resume point in the existing TRACKING format; an implementer never self-certifies an independent audit closure.

## Exposes

**CONTRACT-G08.** Command/browser/native observation recipes with pinned routes/personas, per-action log windows and real setup/cleanup receipts for proven owned disposable environments. Journey findings retain hidden warnings and unknown causes.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/browser/gateway.rs](../../../src/browser/gateway.rs)
- [src/browser/replay.rs](../../../src/browser/replay.rs)
- [src/browser/workflow_mcp.rs](../../../src/browser/workflow_mcp.rs)
- [src/browser/pane.rs](../../../src/browser/pane.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/ui/task_cockpit/config_sidebar.rs](../../../src/ui/task_cockpit/config_sidebar.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Default warm API/data visibility budget is 500 ms unless the agreed project contract overrides it. Every observed action records bounded start/end log offsets. Only explicitly owned disposable targets receive automated setup/reset effects.

## Out of scope

Do not modify application code during a verifier assignment or manufacture an observable pass for a behavior with no usable observation surface.

## Risks & watch-fors

- **Shared browser interference:** Use the workspace/context resource lease and exact human takeover/handback; never silently steal an unrelated authenticated tab.
- **Baseline noise is attributed to this change:** Retain startup/idle and untouched-page comparisons with action offsets; label caused versus pre-existing from evidence rather than severity.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G04, CONTRACT-G05, CONTRACT-G06, CONTRACT-G07 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
