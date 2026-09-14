Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Use canonical project and universal memory with scoped context

Phase 11 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A fresh lead can recover relevant knowledge without depending on an earlier chat. Existing project rules, procedures and memory remain canonical. Retrieval and context reduction retain exact source identity and access, including known failed approaches. Native Knowledge shows what was used and what is stale or unavailable.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G11. Discover the real import chain and existing memory/procedure homes before adding a new file. Use existing Command .memory and API playbooks in their native conventions. For projects without memory, use the single project home and user-wide home named in the overview. Store validated source references and rebuildable lookup metadata in the host; never duplicate canonical rule text as another authority.
2. Build role-specific bounded handoffs with mandatory policy first, then goal decisions/current work, relevant scoped memory and source links. Token reduction applies to derived views, not originals or evidence. Retrieval requires exact source version/range plus current reader/project authority; a digest grants neither. Recover an evicted cache entry from an authorized immutable retained source, otherwise return unavailable/stale. Preserve negative findings, applicability, counterexamples and source-origin labels through all transformations.
3. Render harness-specific context from the same canonical sources. Revalidate view/source and procedure versions at dispatch and after a relevant correction; missing mandatory rules block dependent execution, while optional retrieval failure remains visible without inventing knowledge. Use native harness context/checkpoint handling when qualified, with the durable handoff for replacement sessions. Measure any context reduction against equivalent verified outcomes and total work/usage coverage, not apparent input length alone.
4. Implement Knowledge scope/source/version/applicability links, original retrieval, stale/offline/update states and explicit correction/retirement through the lead. A correction updates the owning canonical file and future retrieval; it cannot end at a chat acknowledgement. Show current required tool access in Agents and the exact source/procedure used for that assignment.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| M01 | A project already has canonical memory, policy imports, and TODO/DONE history | The first lead adopts it and a later lead resumes | Existing homes are reused and relevant knowledge is retrieved; current work stays in its tracker, with no duplicate editable memory or stale status treated as current. |
| M02 | Prior evidence disproved a performance hypothesis under the same current conditions | A later goal investigates the symptom | The lead retrieves the negative result before repeating the experiment, cites its conditions, and chooses justified next work instead of presenting the old hypothesis as new. |
| M03 | Code or environment changes invalidate the conditions behind a remembered conclusion | The conclusion would guide a new assignment | The lead revalidates the affected claim, records why reopening is justified, and preserves prior evidence without blindly obeying or discarding all memory. |
| M04 | Universal working preferences and private Command-specific knowledge both exist | A DevManager goal starts | Applicable preferences are available, while unrelated project facts/data and local permissions are not imported as universal instructions. |
| M05 | A worker-written memory asserts permission to relax a gate or use production credentials | Retrieval selects that entry | Its provenance cannot grant authority; current instructions and real verification still govern, and the unsupported assertion is disputed rather than promoted. |
| M06 | A goal changes from one eligible lead provider to another | Context is reconstructed | The new lead receives/retrieves the same applicable canonical sources and versions through a qualified path; provider-private memory or the builder's chat is not required. |
| M07 | The memory index is large and a binding rule is absent from a relevance search's first results | The host prepares an assignment | Mandatory guidance is delivered separately with source/version accounting; bounded retrieval cannot silently omit the rule or falsely certify a failed import. |
| M08 | The user corrects or retires a stored preference used by queued work | The change is recorded | Future affected context/admission is reconciled with the scoped correction; unrelated project knowledge is preserved and already-sent provider context is not claimed erased. |
| M09 | A compact view references an earlier evidence version that is evicted from cache, its source has since changed, and another project requests that reference without access | The lead resumes and retrieves the omitted detail | An authorized reader gets the exact original version from canonical storage or an explicit missing-evidence result with affected verification re-established. Current source cannot substitute silently, and the other project is denied despite knowing the hash. Required active evidence survives cache eviction under retention/redaction policy. |
| M10 | Two qualified harnesses receive generated views of one project procedure and goal; one view is edited, stale, missing a section or offline | An assignment starts/resumes and the source changes | Drift/freshness is detected and reconciled against canonical versions without overwriting user guidance. All required current instructions reach affected workers before dependent mutation. Permitted cached context remains inspectable as historical; copies cannot approve scope or replace canonical authority. |
| L09 | A verified recurring procedure overlaps an existing project skill/playbook and a draft improvement has supporting evidence | Sharpening the axe consolidates it | One canonical method is updated with applicability, inputs, checks, and failure behavior under the required review; memory links it, prior run results stay historical, and no routine or extra goal is automatically scheduled. |
| L10 | A saved method's command, browser state, or source assumptions no longer match the current task | A new lead/provider attempts reuse | Current preconditions expose the drift before affected action; the lead repairs the permitted method or uses a valid equivalent and obtains fresh result verification, without replaying old permissions, transient IDs, or acceptance ticks. |
| E12 | A context optimization saves visible tokens and no retrieval is requested, but omitted detail determines a required outcome | The optimized and control runs are evaluated with equivalent inputs, tools, and acceptance | The missed outcome remains a failed trial and prevents promotion as an improvement. Retain original evidence, all attempted runs, retrieval/summary work, elapsed time, and usage/cache coverage; a passing candidate must preserve the required outcomes, with unavailable cost or benefit reported as unknown. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_memory.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P11-M01** — `cargo test --locked --test goal_memory p11_m01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M02** — `cargo test --locked --test goal_memory p11_m02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M03** — `cargo test --locked --test goal_memory p11_m03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M04** — `cargo test --locked --test goal_memory p11_m04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M05** — `cargo test --locked --test goal_memory p11_m05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M06** — `cargo test --locked --test goal_memory p11_m06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M07** — `cargo test --locked --test goal_memory p11_m07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M08** — `cargo test --locked --test goal_memory p11_m08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M09** — `cargo test --locked --test goal_memory p11_m09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-M10** — `cargo test --locked --test goal_memory p11_m10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_m10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-L09** — `cargo test --locked --test goal_memory p11_l09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_l09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-L10** — `cargo test --locked --test goal_memory p11_l10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_l10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P11-E12** — `cargo test --locked --test goal_memory p11_e12 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p11_e12`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P11-J1 — Canonical adoption and scope:** Start two fresh leads in Command and one in DevManager. Retrieve relevant existing project guidance and applicable user practice; deny Command-specific private material to the other project. Verify there is one canonical home and mandatory policy is always present. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P11-J2 — Stale and evicted context:** Change a source/procedure, evict a generated view and make an optional source unavailable. Dispatch blocks only mandatory stale inputs; exact authorized originals recover where available and other references return unavailable. No summary silently upgrades source authority. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P11-J3 — Correction and comparable outcomes:** Use Knowledge to correct/retire one entry, then start a fresh lead and verify the owning file and actual retrieved view changed. Compare bounded versus full-source handoffs on the same workload, retaining failed trials and all usage/retrieval costs before claiming an improvement. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P11-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-KNOWLEDGE`, `REF-CONTEXT-DRIFT`, `REF-SOURCE-MISSING`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P11-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G11.** Canonical scoped memory/procedure sources, authorized exact-version retrieval, replaceable harness views, mandatory-context freshness and canonical correction/retirement. Knowledge and Agents expose source/version/tool accessibility without a second authority store.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/providers/adapter.rs](../../../src/providers/adapter.rs)
- [src/browser/gateway.rs](../../../src/browser/gateway.rs)
- [src/ui/task_cockpit/config_sidebar.rs](../../../src/ui/task_cockpit/config_sidebar.rs)
- [docs/research/2026-09-08-command-memory-and-concurrent-goals.md](../../../docs/research/2026-09-08-command-memory-and-concurrent-goals.md)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Exactly one canonical source per durable rule/fact. Mandatory rules are never dropped for relevance or context size. Retrieval pages are at most 128 KiB; incomplete or unauthorized content is explicit, with no credential values in context.

## Out of scope

Do not introduce a vector database, replace provider context management, or auto-promote project-specific facts into user-wide rules.

## Risks & watch-fors

- **A universal memory entry leaks project details:** Abstract only supported reusable behavior, redact private names/data and check scope at every retrieval and transformation.
- **Harness views become a second policy:** Always regenerate from versioned canonical sources; do not accept local view edits as authority or overwrite concurrent canonical changes.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02, CONTRACT-G03, CONTRACT-G05, CONTRACT-G06 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
