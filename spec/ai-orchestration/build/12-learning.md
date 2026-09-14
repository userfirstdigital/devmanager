Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Sharpen rules through coherent evidence-based revisions

Phase 12 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The lead improves durable guidance without reacting to every isolated failure. Related incidents, prior rationale and counterexamples are synthesized into one coherent revision. Publication and later validation remain distinct facts. A concise Axe report can legitimately conclude that nothing needs changing.

## Preconditions

- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) exposes **CONTRACT-G07**. Run `cargo test --locked --test goal_repairs -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [11 — Use canonical project and universal memory with scoped context](11-memory.md) exposes **CONTRACT-G11**. Run `cargo test --locked --test goal_memory -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G12. Consolidate independent incidents by causal mechanism before proposing a process change; repeated messages or retries of one event are one incident. Read the entire owning section and related constraints, preserve successful counterexamples and earlier rationale, and produce a minimal coherent rewrite that removes obsolete duplication. Factual source corrections and explicit user preferences can be applied immediately within their scope; substantive method changes require fresh independent review.
2. Publish with expected canonical source hash and a recoverable file publication journal. Concurrent edits force re-read and semantic reconciliation against the current source; never append a second competing rule or overwrite a newer section. Apply validated project practice locally and require independent evidence from another project before universal promotion. Record changed applicability, rollback/retirement and the evidence behind any reversal to prevent oscillation.
3. At a delivered input version, run knowledge/tracker reconciliation once. Merge unresolved problems into existing clusters after searching DONE recurrence history, then record applied/proposed/deferred/no-change Axe dispositions and canonical links. Do not recursively trigger another delivery or learning run because the Axe report itself was written. Scope constraints, authority and verification integrity cannot be loosened through a learning revision.
4. Knowledge presents the original/current source diff, independent incidents, counterexamples, review status, publication receipt and later validation evidence. “Published” means the source changed; “Validated” requires subsequent observed effect. A regression retires/revises the affected practice with preserved history and current scope.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| L01 | Many worker findings and 300 failing tests all arise from one incident | The axe pass evaluates a proposed universal ban | They count as one incident; the observation is merged by cause and the ban is not adopted from duplicated evidence. |
| L02 | Independent failures and representative successful cases support a more precise shared practice | The lead consolidates guidance | It synthesizes one scoped canonical revision, merges/removes superseded bodies, validates references and behavior, obtains fresh review for the substantive change, and reports evidence/expected benefit. Universal promotion also checks another representative project context. |
| L03 | A later anecdote appears to favor a previously rejected approach | Another lead considers reversing the rule | It consults prior rationale/counterevidence and applicability, retaining or refining the practice until evidence justifies a specific reversal; it does not alternate rules after each incident. |
| L04 | A user explicitly corrects a preference or source inspection proves a referenced file moved | The lead maintains knowledge | The scoped correction is incorporated with evidence and reference validation without demanding a second incident or rewriting unrelated policy. |
| L05 | A proposed learned rule would loosen required tests, permissions, invariants, or a user budget | Consolidation reviews it | It remains a scoped user decision under actual authority; the learning loop cannot approve its own exception or weaken its evaluation criteria. |
| L06 | Two goal leads prepare improvements to the same canonical guidance version | Publication occurs concurrently or is interrupted | The current version and durable lineage are reconciled, both valid contributions are merged coherently, and no last-writer overwrite or torn revision becomes active. |
| L07 | A substantive process revision is activated and later evidence shows a regression | The lead evaluates its effect | The result is recorded, prior rationale is retained, and a justified refinement/revert restores appropriate guidance; a smaller file or isolated success is not falsely called improvement. |
| L08 | A task closes with no durable lesson, or only a provisional candidate lacking evidence | The required ritual runs | Tracker reconciliation and a concise Axe report complete; no arbitrary rule is added and no endless optimization work prevents closure. |
| L11 | Verified delivery changes an API path documented in project memory and also contains one difficult test repair | Close-out runs twice for the same delivered version while a human edits the same knowledge section | Reconcile the affected factual reference from current code, preserve/merge the human edit, and record one durable disposition. The repair is merged with relevant evidence/counterexamples before any broader rule change. Knowledge shows proposed versus actually published/validated state; no duplicate proposal, universal ban or recurring job is created. |
| C04 | An audit discovers an unrelated recurring issue already in the completed tracker | The lead records it | It merges recurrence evidence into the existing problem-tracker conventions without automatically fixing it or duplicating entries. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_learning.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P12-L01** — `cargo test --locked --test goal_learning p12_l01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L02** — `cargo test --locked --test goal_learning p12_l02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L03** — `cargo test --locked --test goal_learning p12_l03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L04** — `cargo test --locked --test goal_learning p12_l04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L05** — `cargo test --locked --test goal_learning p12_l05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L06** — `cargo test --locked --test goal_learning p12_l06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L07** — `cargo test --locked --test goal_learning p12_l07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L08** — `cargo test --locked --test goal_learning p12_l08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-L11** — `cargo test --locked --test goal_learning p12_l11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_l11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P12-C04** — `cargo test --locked --test goal_learning p12_c04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p12_c04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P12-J1 — Synthesis without overreaction:** Submit repeated reports of one incident, two independent incidents, a successful counterexample and an older conflicting rationale. Verify no universal change from the one incident and one coherent independently reviewed project revision from sufficient evidence. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P12-J2 — Concurrent publication and rollback:** Modify the canonical section between proposal and publish. The host refuses stale CAS, reconciles the actual newer text and retains both rationales. Demonstrate a later regression leading to a scoped retirement/revision without oscillating between two appended rules. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P12-J3 — Once per delivery and native result:** Close the same delivered input version twice and replay after restart. Exactly one learning/tracker reconciliation occurs. Inspect factual published, process proposed, later validated and valid no-change Axe states in native Knowledge; a fresh lead uses only current applicable guidance. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P12-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-KNOWLEDGE`, `REF-KNOWLEDGE-STATES`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P12-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G12.** KnowledgeProposal/KnowledgeDisposition, causal incident synthesis, independent process review, canonical CAS publication and once-per-delivered-version reconciliation. Axe states distinguish no-change/proposed/published/validated/retired.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/ui/task_cockpit/config_sidebar.rs](../../../src/ui/task_cockpit/config_sidebar.rs)
- [docs/research/2026-09-08-command-memory-and-concurrent-goals.md](../../../docs/research/2026-09-08-command-memory-and-concurrent-goals.md)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

One incident identity per causal occurrence, regardless of retries. A substantive universal practice needs independent process review and evidence from at least two projects. One reconciliation per delivered input version.

## Out of scope

Do not append a flat lesson list, revise protected authority automatically, or rewrite every project's rules after a single local failure.

## Risks & watch-fors

- **Learning becomes another infinite work queue:** Tie reconciliation to one delivered-version key, record a disposition and end; candidates wait for actual later evidence rather than autonomous background experimentation.
- **An apparent efficiency win harms outcomes:** Evaluate later required acceptance and counterexamples; lower visible token use or a single green run cannot justify wider policy.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G03, CONTRACT-G05, CONTRACT-G06, CONTRACT-G07, CONTRACT-G11 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
