Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Generate complete plans and repair technical dependencies

Phase 03 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The lead turns a finished agreement or clarified outcome into directly buildable work. Small repairs retain a compact plan in the goal record, while substantial work receives the existing overview/phase/test-plan format. Dependencies and source links stay inspectable in the native Plan view. Technical repair preserves valid completed work and does not silently change the agreed outcome.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G03. Read every handed-off source plus canonical constraints, resolve technical choices in the generated documents, and map stable requirement IDs to work and check obligations. Publish the overview, all required phase docs and TEST-PLAN without NEXT prompts. Promote deferred items idempotently to the project’s existing backlog before generation. A docs-only boundary settles after its complete, verified document publication rather than starting a builder.
2. Use an internal typed plan graph, not a user-authored orchestration language. Validate missing dependencies, cycles, unreachable required outcomes, branch/join obligations, contract mismatches and resource self-deadlocks before dispatch. A work item is ready only when its concrete inputs and preconditions exist. Bounded investigation items return the named missing fact and evidence; repeated no-new-evidence investigation uses the same cause/recovery ledger.
3. Represent a whole project as outcomes and vertical milestones across its discovered repos. For a requirement change, obtain the existing real decision, then publish the SPEC amendment and all affected derived documents together through the recoverable publication journal. Technical repair may split/resequence an implementation item and invalidate only its actual dependants. Source restore publishes a new version; it cannot rewind committed effects, pending questions or later evidence.
4. Add source → decision → requirement → work → evidence navigation in Plan using current artifact viewers. Show missing source, repairing plan, preserved complete items and deferred joins. Keep result facts, proposals, contradictory evidence and user decisions distinct in bounded research/transcript summaries. A source unavailable at read time remains explicitly unavailable rather than becoming an invented cached agreement.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| H02 | The same finished directory exists | Robin asks only to generate build docs | The lead generates all required docs and stops at that requested boundary, without starting implementation. |
| N02 | A project brief defines three outcomes across two repos, with one unresolved business decision | Robin requests the whole project | The lead records the known agreement, plans milestones/dependencies, asks only the consequential decision, and executes independent settled work without inventing its answer. |
| N03 | Investigation needs a profiling tool, test command, or internal design choice permitted by project policy | The lead reaches that choice | It uses evidence, makes and records the technical decision, and continues without asking Robin to choose tools, agents, or a workflow template. |
| N05 | A localized defect has a clear intended behavior and no policy requiring a phase pack | The lead plans the repair | A compact goal record contains approach, checks, progress, and findings; the edit and fresh verification proceed without stakeholder docs or unnecessary build phases. |
| N07 | Two project milestones pass but a third required integration outcome remains unmet | The last active worker replies | The lead schedules remaining work or exposes its precise blocker; it cannot declare the project complete or silently deliver only a prototype. |
| N08 | A technical acceptance target was recorded before repair and the repair misses it | Completion is evaluated | The goal remains unmet; the lead cannot lower the target to manufacture success. A proved measurement-method correction preserves the outcome and requires independent validation. |
| N13 | A goal needs an answer from code, a linked article and a supplied meeting transcript, while another slice is settled | The lead investigates and receives conflicting findings plus a stakeholder suggestion | The bounded branch returns cited facts, uncertainty, speaker/source attribution and a recommendation to the owning artifact; independent work continues. A suggestion does not become scope or policy, a focused answer needs no new report, and the user does not relay agent output. |
| U19 | A native goal has cited research, a revised dependency, a stale harness view, a pending measurement and a knowledge update | The user follows an item through Plan, Checks, Knowledge and Agents, then returns to the conversation at normal and narrow geometry | Current source/decision/implementation/evidence links open real viewers with focus/return preserved. Missing/stale sources, repairing-plan, context-update, measurement and knowledge-publication states match host facts; no placeholder or extra graph/sidebar substitutes for the feature. Functional, journey and matched native appearance evidence cover the slice. |
| G01 | A finished spec has three phases and no `build/` | Generation starts | Overview, all three phase docs, and TEST-PLAN are produced without `NEXT`; every spec scenario maps to a phase/check. |
| G02 | A previous build doc exposes `/exports`, but a later draft assumes `/downloads` | Generation reconciles docs | The mismatch is resolved within the agreed contract or raised as a scoped product decision before dependent implementation. |
| G03 | Two parked ideas already exist in the shared backlog | Generation is rerun | Those entries are not duplicated or added to implementation scope. |
| G04 | A phase draft says “investigate and choose an approach” | Generation performs its close-out | The technical approach is settled in the doc, or a specific unresolved empirical/product blocker is recorded before the phase is called buildable. |
| G05 | Evidence disproves an internal implementation assumption while the agreed behavior and external contracts remain achievable | A builder reports the contradiction | The lead chooses a permitted alternative, updates all affected build docs/assignments/check mappings, and resumes without asking Robin to make the engineering choice. |
| G06 | Independent work has already passed valid checks when a dependent implementation plan changes | The lead replans | Valid completed work is retained; obsolete assignments/evidence are reconciled; the whole feature is not blindly restarted. |
| G07 | Proposed assignments form a dependency cycle or a waiting parent holds the only capacity its helper needs | The lead/host validates the work | The technical decomposition or schedule is repaired before further admission; the run does not deadlock under a generic Running state. |
| G08 | Two prerequisite branches need separate decisions and another branch has completed with valid evidence | The first decision is answered, then the host restarts before the second answer | Completed work is retained, the remaining decision stays pending, and the join waits for both current answers and satisfactory branch results. The later answer resumes only eligible work; an empty queue or handled prerequisite failure cannot complete the join, and settled effects are not repeated. |
| G09 | A requirement and its evidence link to source version 2; a current run has already performed an external action | An authorized source edit or restore changes the current agreement, or the original source becomes unavailable | Inspect the source-to-decision-to-plan-to-evidence trail. The change creates a new version, preserves historical references/effects, reconciles affected work and invalidates stale proof. An unavailable source is labeled; no fabricated content, automatic approval, competing agreement or execution rollback occurs. |
| G10 | A generated/repaired plan contains a missing edge, a cycle, sparse or duplicate IDs, an in-progress parent with an unmet prerequisite, or a done parent with an unverified required child | Scheduling or regeneration is attempted | The lead resolves the technical defect before dependent admission, validates stable identity and required coverage, and preserves valid completed work. Removing an edge, changing a label or reusing array positions cannot satisfy a requirement; a default complexity estimate cannot force unnecessary tasks. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_plans.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P03-H02** — `cargo test --locked --test goal_plans p03_h02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_h02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-N02** — `cargo test --locked --test goal_plans p03_n02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_n02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-N03** — `cargo test --locked --test goal_plans p03_n03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_n03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-N05** — `cargo test --locked --test goal_plans p03_n05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_n05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-N07** — `cargo test --locked --test goal_plans p03_n07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_n07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-N08** — `cargo test --locked --test goal_plans p03_n08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_n08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-N13** — `cargo test --locked --test goal_plans p03_n13 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_n13`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-U19** — `cargo test --locked --test goal_plans p03_u19 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_u19`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G01** — `cargo test --locked --test goal_plans p03_g01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G02** — `cargo test --locked --test goal_plans p03_g02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G03** — `cargo test --locked --test goal_plans p03_g03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G04** — `cargo test --locked --test goal_plans p03_g04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G05** — `cargo test --locked --test goal_plans p03_g05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G06** — `cargo test --locked --test goal_plans p03_g06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G07** — `cargo test --locked --test goal_plans p03_g07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G08** — `cargo test --locked --test goal_plans p03_g08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G09** — `cargo test --locked --test goal_plans p03_g09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P03-G10** — `cargo test --locked --test goal_plans p03_g10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p03_g10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P03-J1 — Document output:** Hand over a finished three-slice spec as docs-only. Verify complete overview, three buildable phase documents and TEST-PLAN, exact shared contracts, no planning deliverable and no implementation start. Re-run and verify the parked backlog has no duplicate. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P03-J2 — Semantic plan repair:** Inject a cycle, missing edge, unreachable required check, stale source, invalid join and resource self-dependency into separate typed proposals. Each returns the specific validation error and dependent wait; a valid repaired version preserves previously verified independent work. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P03-J3 — Lineage and project steering:** Use a project brief with a product decision plus technical unknown, then a changed source version. In Plan open each source/decision/work/check link, restore an old document as a new version, and verify only affected work/evidence becomes stale. Complete ordinary technical decisions without a human NEXT. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P03-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-PLAN`, `REF-LINEAGE`, `REF-SOURCE-MISSING`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P03-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G03.** PlanItem/InvocationKey/RequirementRef, complete canonical build-doc publication, semantic graph validation, versioned source/decision/work/evidence links and recoverable PublicationRecord. Plan query exposes real dependency/repair/source-unavailable state.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/workspace/repository_targets.rs](../../../src/workspace/repository_targets.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)
- [src/ui/task_cockpit/config_sidebar.rs](../../../src/ui/task_cockpit/config_sidebar.rs)
- [spec/ai-orchestration/UX-SPEC.md](../../../spec/ai-orchestration/UX-SPEC.md)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Plan query pages contain at most 200 items. One canonical agreement per run lineage. Each required outcome has at least one work item and one observable check. No ready item has unresolved design or dependency entries.

## Out of scope

Do not create a second SPEC or silently redesign the agreed API contract. Do not generate a large build pack for a localized repair that fits the canonical goal record.

## Risks & watch-fors

- **Partly published document set:** Use staged content, expected old hashes and publication receipts; reconcile partial publication before dispatching any work that reads that set.
- **Project scope expands through research:** Keep facts/proposals separate and apply only the existing goal agreement or explicit amendment; unresolved product changes remain scoped decisions.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
