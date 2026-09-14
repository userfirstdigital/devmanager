Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Repair the full requested failing-test inventory

Phase 10 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A large red suite becomes a tracked campaign over exact test identities. The lead repairs shared causes while preserving requested coverage and independently verifying the final inventory. Newly exposed failures and standing red cases remain correctly scoped. The native Checks view explains what has actually been resolved and what remains.

## Preconditions

- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) exposes **CONTRACT-G07**. Run `cargo test --locked --test goal_repairs -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G10. Complete discovery and the baseline invocation before treating its inventory as complete; capture test project/file/full name/parameter identity plus framework version, selected filters, disabled/skipped cases and terminal summary. Keep original failures, newly exposed in-scope failures and new regressions separately identifiable. Requested pre-existing red cases are in scope; unrelated pre-existing problems are merged into the normal tracker.
2. Group by demonstrated root cause and assign bounded repairs through phase 7, not one worker per assertion. Track both membership and causal lineage so group renaming cannot reset attempts. Compare failing identity sets before/after rather than counts, detect silently excluded files/tests, and reconcile newly introduced integration failures against the final complete suite.
3. Verify tests exercise actual behavior and required auth/tenant boundaries. Never obtain green by skipping checks, mocking away the thing tested or changing expected output to match broken behavior. A proved incorrect test/setup may receive the smallest correction only when current project policy permits it, with explicit evidence of intended coverage and a fresh independent review. An uncertain acceptance requirement remains a scoped decision.
4. Treat OOM, harness crash and missing terminal summary as incomplete and preserve the exact process ownership before resource-adjusted retry. For flakes, run at least twenty repeats under retained seed/order/conditions and then the complete selected suite; use more evidence if the observed failure rate makes twenty inadequate. Native Checks shows original/current counts derived from identities, per-cause history, new failures and current final reconciliation.

Extend the phase-08 fixture seed with 300 actual executable failing unittest cases across three shared causes, plus verifier-owned exclusion/substitution/no-summary/seeded-flake variants. The manifest pins each case identity and the real command `python3 -m unittest discover -s tests -v`. The target agent edits source under its allowed paths; fault controls and original expected inventories remain verifier-owned. This corpus is synthetic project acceptance with real tests; real Command maintenance remains separately required.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| T01 | The supplied report says 300 failures, but the suite OOMs without completion | The lead begins “fix these red tests” | It records an incomplete baseline, repairs permitted setup/resource conditions, and obtains the actual failure inventory rather than treating the crash or reported count as a completed result. |
| T02 | A completed baseline contains 300 failing cases, many sharing several root causes | The lead decomposes the campaign | It groups evidenced causes, assigns bounded repairs, preserves case identities and dependencies, and avoids one worker per case or overlapping shared-state writes. |
| T03 | All requested failures predate this run and a project report calls them a standing red baseline | The lead evaluates scope | Those failures remain required work; their pre-existing label cannot exempt them or make an unchanged red suite complete. |
| T04 | A group fails because application authorization is broken | A worker proposes mocking the authorization boundary | The proposal is rejected, application behavior is repaired, and real checks plus independent verification establish closure. |
| T05 | A fixture path or assertion is demonstrably wrong under an authoritative current contract | The lead repairs the campaign | The test/setup is corrected with preserved intended coverage and independent evidence; no procedural human approval is needed. Repeat with ambiguous intended behavior: that case becomes a scoped product question. |
| T06 | A proposed repair skips cases, narrows discovery, deletes assertions, or automatically accepts broken snapshots | Campaign verification compares scope and coverage | The lost checks are detected, original failure identities remain unresolved, and a green reduced run cannot close the goal. |
| T07 | The final failing count equals the baseline after a repaired case is replaced by a newly failing identity | Results are reconciled | The new failure is detected and repaired when required; counts alone do not establish progress or absence of regression. Renamed cases retain explicit lineage. |
| T08 | A known intermittent test happens to pass once | The lead evaluates closure | The known seed/order/concurrency is exercised repeatedly under the recorded sampling rule, the full requested suite runs, and unsupported claims of eliminating flakiness are rejected. |
| T09 | Four distinct causes have been independently repaired but hundreds of cases remain under other causes | The lead schedules more work | It continues within the run budget; the four-attempt limit is not a campaign-wide stop. A persistent cause cannot gain new attempts by splitting its symptom IDs. |
| T10 | Independent repair batches pass narrowly but their combined changes break the requested suite | Integration verification runs | The regression becomes required repair, and no campaign success is issued from isolated batch passes. |
| T11 | A common setup fix exposes more failures in the requested suite, while an unrelated pre-existing issue exists outside a specifically named subset | The lead reconciles scope | Newly exposed in-scope failures remain required work; the unrelated issue retains its tracker disposition rather than causing a silent scope rewrite. |
| T12 | All requested original failures have verified resolution and the complete requested suite/build gates pass on final inputs | Fresh verification closes the campaign | The report links the original inventory, cause repairs, current suite evidence, and revisions; missing access or any remaining required failure would instead produce an incomplete outcome. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_test_campaigns.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P10-T01** — `cargo test --locked --test goal_test_campaigns p10_t01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T02** — `cargo test --locked --test goal_test_campaigns p10_t02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T03** — `cargo test --locked --test goal_test_campaigns p10_t03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T04** — `cargo test --locked --test goal_test_campaigns p10_t04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T05** — `cargo test --locked --test goal_test_campaigns p10_t05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T06** — `cargo test --locked --test goal_test_campaigns p10_t06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T07** — `cargo test --locked --test goal_test_campaigns p10_t07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T08** — `cargo test --locked --test goal_test_campaigns p10_t08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T09** — `cargo test --locked --test goal_test_campaigns p10_t09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T10** — `cargo test --locked --test goal_test_campaigns p10_t10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T11** — `cargo test --locked --test goal_test_campaigns p10_t11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P10-T12** — `cargo test --locked --test goal_test_campaigns p10_t12 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p10_t12`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P10-J1 — Large real inventory:** Create a 300-failing-test owned fixture with shared causes and actually run it. Repair by cause, retain original identities and newly exposed in-scope cases, and reconcile a complete independently run final suite with zero unresolved requested failures. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P10-J2 — False-green attacks:** Substitute one failing test for another while preserving the count, remove a file from discovery, skip a boundary test, mock authorization and produce an OOM without a summary. Each remains failed/incomplete or creates a blocking integrity finding. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P10-J3 — Legitimate correction and flake:** Prove a setup/test defect while preserving intended coverage and review it independently. Exercise a deterministic seeded flaky fixture with the repeat policy and full-suite confirmation. Inspect every cause and actual receipt from native Checks. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P10-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-TESTS`, `REF-CHECK-COVERAGE`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P10-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G10.** Complete exact test inventories, root-cause campaign membership, requested/new/regression scope, integrity checks, flake evidence and final independent suite reconciliation. Native Counts are derived from retained identities and current receipts.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)
- [AGENTS.md](../../../AGENTS.md)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

All original requested identities plus new in-scope failures and introduced regressions must reconcile. Minimum 20 comparable repeats for flake evidence, followed by the complete requested suite; no-summary runs are incomplete.

## Out of scope

Do not quarantine requested red tests, compare only failure counts, or spawn a worker for every test.

## Risks & watch-fors

- **A 300-case fan-out overwhelms shared state:** Use cause groups and actual resource leases; command count and green progress are derived, not a reason to exceed runtime limits.
- **A standing red baseline becomes an excuse:** The user’s requested failure set is required acceptance; only genuinely unrelated pre-existing failures stay outside the campaign.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G03, CONTRACT-G04, CONTRACT-G06, CONTRACT-G07 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
