Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Build and integrate across owned repository workspaces

Phase 04 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

Independent implementation work can run without corrupting another writer’s checkout. Each assignment identifies all participating repositories and lands only its own change. Command’s multi-repository layout uses the same mechanism as another project. Native Plan, Agents and Changes expose the real workspaces, integration state and retained edits.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G04 with existing WorkspaceService, repository targets, Git service and process manager. Resolve nested roots and Windows/Linux aliases to stable local identities. Use a run-owned sibling workspace containing a worktree for each affected Git root; non-Git root artifacts remain separately owned and versioned. Acquire normalized path/repo resource leases before write dispatch. If actual project policy requires a shared checkout, serialize its writers instead of evading that rule with an extra worktree.
2. Extend specialist admission to writable goal assignments through the new authorized goal path. Tie each worker to its bounded allowed paths, base revisions, task/run/attempt and exact runtime resource. Enforce process/workspace access through the qualified harness and supported sandbox, not only a prompt. Native children with unobservable counts are shown as unknown and do not invent top-level runtime receipts.
3. Adopt task work only after verifying the exact worker wrapper and all descendants settled or remain deliberately owned. Read the diff and actual check receipts, reconcile changed bases, preserve foreign staged/unstaged/untracked files, then integrate with explicit pathspecs. Resolve ordinary merge conflicts within the agreement; re-verify affected inputs. Record landing per repository, including partial failure, instead of presenting cross-repo Git as one atomic commit.
4. Use the project’s current landing granularity and required review, not a hardcoded master or per-phase ritual. Keep [b] built versus [x] independently verified tracking. Plan lists participating roots; Changes opens the selected repo/revision; Agents shows ownership and blocked overlapping writes. A stale late worker result cannot attach to a replacement attempt.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| I01 | A feature has three dependent phases and actual landing policy permits one feature commit | Implementation proceeds | Phases build in order with truthful boundary updates; the required feature review, checks, and landing occur without per-phase human handoffs. |
| I02 | TRACKING marks a scenario `[b]`, but its acceptance test has not run | The lead reports progress | It remains built but unverified; it becomes `[x]` only with actual acceptance evidence. |
| I03 | An in-scope edit introduces a failing authorization check | Tests run | The code is fixed; the check is not weakened, skipped, or mocked away. |
| W01 | A non-Git root contains independent `/api` and `/web` repos | A vertical slice is assigned | Both actual repo roots and source versions are bound; the worker sees the required sibling layout and shared spec/policy context. |
| W02 | Two writable assignments are eligible under a supported capacity policy | They run concurrently | They use separately owned workspaces, preserve unrelated user edits, and integrate through one owner with checks against the combined result. |
| W03 | `/api` commits successfully but `/web` landing fails | The lead prepares completion | Partial landing stays visible and is reconciled; the feature is not reported landed everywhere. |
| W04 | An old auditor result refers to an earlier attempt or document version | It arrives after the replacement attempt started | It is retained as historical evidence and cannot overwrite the current audit or advance the run. |
| W05 | Independent assignments need the same test database, port, or exclusive compiler lane | Both become eligible | One acquires the resource and the other waits on its actual release; no duplicate build or cross-run test-state mutation occurs. |
| W06 | The integration base changes and a merge conflict includes user-owned edits | The lead lands a feature | It preserves those edits, resolves permitted technical conflicts, rechecks affected combined behavior, and records any real product conflict without force-rewriting unrelated history. |
| J01 | Command has a root workspace repo, independent code repos, name aliases, and a stale note denying the extension's own Git root | The lead prepares a Command goal | It resolves actual roots and aliases from evidence, corrects the scoped fact/reference, and does not create a stray alias directory or infer application cleanliness from root Git status. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_workspaces.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P04-I01** — `cargo test --locked --test goal_workspaces p04_i01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_i01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-I02** — `cargo test --locked --test goal_workspaces p04_i02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_i02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-I03** — `cargo test --locked --test goal_workspaces p04_i03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_i03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-W01** — `cargo test --locked --test goal_workspaces p04_w01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_w01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-W02** — `cargo test --locked --test goal_workspaces p04_w02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_w02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-W03** — `cargo test --locked --test goal_workspaces p04_w03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_w03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-W04** — `cargo test --locked --test goal_workspaces p04_w04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_w04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-W05** — `cargo test --locked --test goal_workspaces p04_w05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_w05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-W06** — `cargo test --locked --test goal_workspaces p04_w06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_w06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P04-J01** — `cargo test --locked --test goal_workspaces p04_j01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p04_j01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P04-J1 — Two repos and foreign edits:** Use a non-Git root with independent api/web repos and a pre-existing human edit. Build one vertical behavior, change a base while a disjoint worker runs and reconcile. Verify the foreign edit survives, exact pathspecs were used and fresh evidence matches each landed revision. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P04-J2 — Writable specialist ownership:** Run two disjoint assignments and attempt a third overlapping assignment. Verify actual permission/lease admission, no overlap, a stale callback refusal, and exact wrapper/child-tree ownership after the worker reports done. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P04-J3 — Partial integration:** Force the second repo landing to fail after the first succeeds. Native Changes shows both actual states, the goal stays incomplete, and resume reconciles only the missing landing without duplicating the first. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P04-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-PLAN`, `REF-AGENTS`, `REF-RUN`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P04-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G04.** AssignmentSpec/RepoInput/ResourceRequest and writable specialist admission through the goal authority path; isolated multi-repo ownership, current-input reconciliation and per-repo integration receipts. Changes and Agents show exact roots and owners.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/workspace/service.rs](../../../src/workspace/service.rs)
- [src/workspace/worktree.rs](../../../src/workspace/worktree.rs)
- [src/workspace/repository_targets.rs](../../../src/workspace/repository_targets.rs)
- [src/git/git_service.rs](../../../src/git/git_service.rs)
- [src/domain/command.rs](../../../src/domain/command.rs)
- [src/providers/orchestrator.rs](../../../src/providers/orchestrator.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/ui/task_cockpit/config_sidebar.rs](../../../src/ui/task_cockpit/config_sidebar.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

One writer per overlapping resource set. Every Git read child sets GIT_OPTIONAL_LOCKS=0; mutation children retain normal locking. Each current repo result identifies a base, resulting tree/revision and owned diff.

## Out of scope

Do not commit another session's changes, migrate shared databases, or turn a source commit into a deployed-state claim.

## Risks & watch-fors

- **Generated inputs missing in isolated worktrees:** Restore the verified ignored inputs before compiler admission and bind CARGO_TARGET_DIR beneath that worktree; never share the daily checkout target.
- **Concurrent human edits:** Use expected base plus dirty/generated input fingerprints before integration; preserve foreign work and renew verification against the actual result.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02, CONTRACT-G03 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
