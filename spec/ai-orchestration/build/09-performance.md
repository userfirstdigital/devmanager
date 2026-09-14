Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Diagnose and repair measured performance outcomes

Phase 09 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A slow-page report starts the same goal workflow without needing a prepared SPEC. The lead reproduces the real interaction, identifies recoverable cost and verifies its improvement. Measurements use the target project’s actual conditions. Native Checks shows comparable before/after evidence and functional correctness, including an honest missed or unverified target.

## Preconditions

- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) exposes **CONTRACT-G07**. Run `cargo test --locked --test goal_repairs -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [08 — Verify real user and operator journeys in owned environments](08-journeys.md) exposes **CONTRACT-G08**. Run `cargo test --locked --test goal_journeys -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G09. Capture the target interaction, baseline conditions and success budget before repair. For diagnose-only goals, deliver the evidence-backed cause and proposed fix without code mutation; for fix goals, dispatch bounded changes ranked by recoverable cost. If the page or persona is ambiguous, route one actionable clarification and continue independent inspection.
2. For the generic warm profile, run three warmups and twenty measured samples before and after, retaining every sample and nearest-rank p95. Independently repeat the final twenty measured samples after three warmups. Use three separately reported cold starts when cold startup is relevant; do not invent a p95 from three observations. Pin dataset, auth, route, build, tool, cache, network and CPU conditions and reject incomparable samples. Preserve negative hypotheses and moved costs across affected journeys.
3. For Command, adopt its current canonical speed/measuring playbooks: production assets served locally, valid auth, empty HTTP/module caches, Chrome MCP, 10 Mbps down/40 ms RTT/4× CPU, five navigations per round, median and spread, cold load below 300 ms. Recheck the source at dispatch; the generic warm budget does not override it. Provider/tool authority and a user’s live authenticated browser remain subject to current ownership and secure handoff.
4. Show target/conditions, sample count/distribution, baseline/current delta, rejected hypotheses, remaining cost and regression result in the existing Checks detail. A claim that “it feels faster,” a changed test setting, hidden data, disabled authorization or an out-of-scope future production measurement cannot complete a requested local measurable outcome.

Extend the phase-08 fixture corpus with the controlled sequential-request latency and pinned realistic dataset described in TEST-PLAN. Retain real served request/data timing observations and functional guards; do not synthesize the performance receipt from the injected delay setting. The generic baseline/after/fresh-final rounds each have their own three warmups and twenty retained measurements.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| N01 | The active project and reported page are identifiable, but no spec exists | Robin says “This page is too slow; fix it” | The lead creates the concise goal record, reproduces/measures the problem, and proceeds through repair and independent verification without requesting a spec or LOCKED marker. |
| N06 | Two equivalent reports have respectively authorized “find the cause” and “fix the problem” | An evidenced cause is found | The first can complete with the diagnosis; the second continues to repair and verify without another handoff. |
| J03 | Command's applicable speed playbook specifies its cold-load profile and sampling rule | A slow-page goal begins | Those verified project requirements govern measurements; the generic warm p95 default cannot replace them, and unavailable required instrumentation remains a specific capability gap. |
| P01 | A reproducible warm interaction takes several seconds with representative data and no custom target | The lead owns the slow-page repair | It records the visible completion metric, baseline, applicable 500 ms target, and cause evidence; it fixes and independently measures the same interaction on final integrated inputs. |
| P02 | An N+1 query is suspected, but traces show rendering dominates the delay | Investigation tests the hypothesis | The lead records the rejected explanation, investigates the observed bottleneck, and repairs it without asking Robin to redesign the plan or claiming the query hypothesis proved. |
| P03 | The reported problem concerns cold navigation or a legitimate long bulk operation | The lead sets acceptance | It records an appropriate numeric target/measurement recipe and distinguishes cold from warm behavior; the 500 ms data-response default is not misapplied to total cold build or bulk duration. |
| P04 | A proposed comparison changes data volume, cache preparation, or build mode between baseline and final | Verification assesses improvement | The comparison is rejected or rerun under comparable recorded conditions; a faster incomparable result cannot close the goal. |
| P05 | One fast sample meets the target but the recorded repeated measurements do not | The lead evaluates performance | It retains all samples and reported distribution, independently repeats the final set, and keeps the goal open instead of choosing the fastest run. |
| P06 | A proposed speed fix removes returned data, weakens access checks, hides errors, or only paints a spinner sooner | Functional and outcome verification run | The shortcut fails acceptance despite a faster superficial metric; required behavior and useful-content timing are preserved. |
| P07 | The symptom requires unavailable conditions and cannot be reproduced or established from authorized evidence | Investigation exhausts permitted alternatives | The lead records the precise observation/access gap and preserved experiments; it does not claim a successful repair from an unrelated local benchmark. |
| P08 | A repair speeds reads by moving work into writes or a background task | Final verification runs | The reported interaction and affected correctness/states pass, moved costs are measured against relevant requirements, and a new blocking regression prevents completion. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_performance.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P09-N01** — `cargo test --locked --test goal_performance p09_n01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_n01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-N06** — `cargo test --locked --test goal_performance p09_n06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_n06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-J03** — `cargo test --locked --test goal_performance p09_j03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_j03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P01** — `cargo test --locked --test goal_performance p09_p01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P02** — `cargo test --locked --test goal_performance p09_p02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P03** — `cargo test --locked --test goal_performance p09_p03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P04** — `cargo test --locked --test goal_performance p09_p04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P05** — `cargo test --locked --test goal_performance p09_p05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P06** — `cargo test --locked --test goal_performance p09_p06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P07** — `cargo test --locked --test goal_performance p09_p07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P09-P08** — `cargo test --locked --test goal_performance p09_p08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p09_p08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P09-J1 — Measured repair:** Use a fixture page with a reproducible sequential-request delay. Pin conditions, run the complete generic baseline, make a bounded repair, collect the full after samples and independent final repeat, and pass the affected functional regression journey. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P09-J2 — Invalid measurement:** Change cache/network/data/build between before and after, remove an inconvenient slow sample, and move latency to another required action. Each invalidates the performance claim or exposes the regression; no missing baseline is guessed. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P09-J3 — Command and diagnose-only:** Exercise the actual Command playbook against an authorized isolated current build with its Chrome/production/cold conditions. Separately issue “diagnose this slow page” and verify no source mutation. Native Checks distinguishes achieved, FLOOR and Not verified results. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P09-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-SPEED`, `REF-DELIVERY-PENDING`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P09-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G09.** Performance observation artifacts in CheckReceipt: pinned target/conditions, complete comparable samples, generic warm p95/cold sample rules and actual project overrides. Diagnose-only and measured repair boundaries share the existing Plan/Checks flow.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/browser/replay.rs](../../../src/browser/replay.rs)
- [src/browser/gateway.rs](../../../src/browser/gateway.rs)
- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)
- [docs/research/2026-09-08-command-memory-and-concurrent-goals.md](../../../docs/research/2026-09-08-command-memory-and-concurrent-goals.md)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Generic: 3 warmups + 20 baseline samples + 20 after samples and a fresh 3-warmup/20-sample final repeat; p95 is sorted sample 19 of 20. Cold exploratory profile: 3 individual starts. Command: 5 cold navigations each round, median and spread, <300 ms under its pinned profile.

## Out of scope

Do not introduce a separate performance workflow engine, cache away required behavior, or weaken correctness/authorization to meet a timing number.

## Risks & watch-fors

- **Contention hides the real bottleneck:** Record concurrent workload and the project’s saturation guard; mark affected numbers incomparable and retain the prerequisite for a valid repeat.
- **Project playbook contains stale command descriptions:** Resolve each command from current manifest plus canonical policy, record factual drift in its owning source, and run the actual allowed command without assuming the stale description passed.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G03, CONTRACT-G04, CONTRACT-G06, CONTRACT-G07, CONTRACT-G08 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
