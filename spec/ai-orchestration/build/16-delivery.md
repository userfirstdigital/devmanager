Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Finish reviewed delivery with current outcomes and evidence

Phase 16 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The final account states what the goal actually achieved at its authorized boundary. Verified implementation, independent review and the final delivery action are separate ordered obligations. A commit awaiting deployment is described accurately. Current evidence, unresolved work and the Axe disposition remain linked from the same native Task.

## Preconditions

- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) exposes **CONTRACT-G07**. Run `cargo test --locked --test goal_repairs -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [08 — Verify real user and operator journeys in owned environments](08-journeys.md) exposes **CONTRACT-G08**. Run `cargo test --locked --test goal_journeys -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [09 — Diagnose and repair measured performance outcomes](09-performance.md) exposes **CONTRACT-G09**. Run `cargo test --locked --test goal_performance -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [10 — Repair the full requested failing-test inventory](10-test-campaigns.md) exposes **CONTRACT-G10**. Run `cargo test --locked --test goal_test_campaigns -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [11 — Use canonical project and universal memory with scoped context](11-memory.md) exposes **CONTRACT-G11**. Run `cargo test --locked --test goal_memory -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [12 — Sharpen rules through coherent evidence-based revisions](12-learning.md) exposes **CONTRACT-G12**. Run `cargo test --locked --test goal_learning -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [13 — Run multiple task leads under fair shared ownership](13-scheduling.md) exposes **CONTRACT-G13**. Run `cargo test --locked --test goal_scheduling -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [14 — Recover from profile limits without changing authority](14-availability.md) exposes **CONTRACT-G14**. Run `cargo test --locked --test goal_availability -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [15 — Resume exact work after interruption and reconcile effects](15-recovery.md) exposes **CONTRACT-G15**. Run `cargo test --locked --test goal_recovery -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G16. Derive required obligations from the agreement, all active plan branches, in-scope findings and explicit delivery boundary. Require current implementation/outcome receipts first, then fresh independent review, then the authorized final landing/deployment action and its actual receipt. Reviewers do not have to prove their own future approval; a passed review does not waive an outstanding delivery effect.
2. Acquire the final resource/input fence across every affected repository, generated artifact, environment/data condition, guidance/method and visual component/target. Reconcile queued and in-flight writes and invalidate only affected evidence on changed inputs. A reviewer vote, empty queue, worker exit, manual Done action or stale screenshot cannot satisfy this predicate. Unknown external outcomes or not-verifiable required checks keep delivery incomplete.
3. Land per current project policy and per-repo receipt; report partial multi-repo landing honestly. If the boundary is ready-to-deploy, retain the exact run-sheet/manual actions without claiming deployment. If deployment or an immediate measurable outcome is in scope and authorized, obtain the actual effect/measurement receipt. Future out-of-scope business impact is distinct from an observable local result and is never invented as a measured causal improvement.
4. Complete owned cleanup or an explicitly recorded preserved-work disposition, reconcile tracker and knowledge once for the delivered input version, then publish the final account. The native result links actual revisions, checks, independent disposition, outcome measurements, remaining actions and Axe report. A follow-up uses a new run/version in the same Task while retaining the preceding delivered outcome.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| C01 | All required checks and independent closures cover the final revisions | The lead closes the run | It reports SHIPPABLE with prescribed commits/evidence and remaining authorized manual delivery steps, without claiming those steps executed. |
| C02 | Every worker has replied, but a required journey is NOT VERIFIED | The lead attempts completion | It reports the precise incomplete condition; SHIPPABLE is withheld. |
| C06 | A failed journey needs its database state for diagnosis and a run-owned service still holds a port | The run resets, stops, or completes | Reproducible evidence is retained before owned cleanup; unrelated resources survive, and failed cleanup remains visible rather than being reported settled. |
| C07 | All checks passed but an accepted write or changed base can still invalidate their inputs | The lead tries to publish the final verdict | Outstanding work and input versions are reconciled before completion; a later edit preserves the earlier verdict as history and requires new affected evidence. |
| C08 | Two independent reviewers report completion while a required check is missing/failed, evidence is stale, or an unresolved required finding remains | The host evaluates completion | SHIPPABLE is refused despite the votes or stop flags. The lead reconciles the contradictory evidence through the normal verification/fix path; only current required evidence and independent dispositions can satisfy the obligation. |
| C09 | The user requested and authorized a final PR or other delivery action after independent review | The implementation review passes, but final delivery has not succeeded | Review readiness can be established without pretending the later action already happened. The host verifies the review-process obligation, performs the authorized final action, reconciles uncertain results and withholds full delivery until its receipt exists. A ready-to-deploy boundary still leaves production deployment explicitly manual. |
| C10 | A software goal has completed all planned tasks; its agreement either ends at ready-to-deploy or requires a measured improvement | The lead evaluates completion with only a PR, uncomparable samples or an observation period not yet complete | Ready-to-deploy can finish when its own checks pass while production outcomes stay explicitly unmeasured. A required improvement remains unverified until the agreed measure/conditions are met. Task counts, confidence and shipped code cannot establish the result or causality; no unauthorized deployment or hidden target reduction occurs. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_delivery.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P16-C01** — `cargo test --locked --test goal_delivery p16_c01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P16-C02** — `cargo test --locked --test goal_delivery p16_c02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P16-C06** — `cargo test --locked --test goal_delivery p16_c06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P16-C07** — `cargo test --locked --test goal_delivery p16_c07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P16-C08** — `cargo test --locked --test goal_delivery p16_c08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P16-C09** — `cargo test --locked --test goal_delivery p16_c09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P16-C10** — `cargo test --locked --test goal_delivery p16_c10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p16_c10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P16-J1 — Ordered obligations:** Verify implementation, complete independent review and delay the final second-repo landing. The goal shows review passed/delivery pending; it closes only after the actual authorized landing receipt. Repeat with a ready-to-deploy boundary and show manual deployment explicitly. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P16-J2 — Final fence:** Alter an affected source/generated file, environment condition, shared UI component or reference target during final review; queue another writer. Final delivery refuses stale evidence until settlement and the affected checks/visual proof are current. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P16-J3 — Outcome and native follow-up:** Complete a measured local result and an outcome with later out-of-scope production impact. The final account distinguishes observed from causal/unmeasured claims, includes cleanup/Axe disposition, and starts a scoped follow-up without losing the previous delivered version. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P16-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-DONE`, `REF-DELIVERY-PENDING`, `REF-KNOWLEDGE-STATES`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P16-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G16.** GoalDelivery and its current-input final fence, ordered implementation/review/final-effect obligations, per-repo/boundary outcome receipts and cleanup/knowledge disposition. Final account and follow-up preserve the actual achieved version.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/operation.rs](../../../src/domain/operation.rs)
- [src/domain/event.rs](../../../src/domain/event.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/git/git_service.rs](../../../src/git/git_service.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)
- [src/ui/quality.rs](../../../src/ui/quality.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Zero unconditional completions with unmet required outcomes, open required findings, stale receipts or unsettled effects. At most one final publication/reconciliation per delivered input vector. All participating repository results are explicit.

## Out of scope

Do not equate ready-to-deploy with deployed, let reviewer votes waive required behavior, or delay every goal for future business metrics outside its agreed boundary.

## Risks & watch-fors

- **Final review depends on future delivery:** Use three explicitly ordered obligation kinds and do not make review self-referential; final effects have their own receipts after review.
- **A clean source tree hides changed evidence inputs:** Fence the complete recorded input vector, including generated assets, tool/data conditions and shared visual sources, not just HEAD.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G03, CONTRACT-G04, CONTRACT-G05, CONTRACT-G06, CONTRACT-G07, CONTRACT-G08, CONTRACT-G09, CONTRACT-G10, CONTRACT-G11, CONTRACT-G12, CONTRACT-G13, CONTRACT-G14, CONTRACT-G15 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
