Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Run multiple task leads under fair shared ownership

Phase 13 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

Several goals can make progress without competing blindly for the same files, browser or test environment. Each goal retains its own lead, questions and memory scope. The host admits work fairly under shared limits. The existing board explains active and waiting work without becoming another scheduler interface.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [11 — Use canonical project and universal memory with scoped context](11-memory.md) exposes **CONTRACT-G11**. Run `cargo test --locked --test goal_memory -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G13 as one bounded admission owner in the existing host executor. Count actual top-level lead/worker resources across tasks and enforce the intersection of user/host/project/provider limits. Default to two executing goals, at most two top-level runtimes per goal including its lead, and four globally. Unknown native child counts remain explicitly unknown. Read-only eligible investigation can continue when a particular write/effect lease is unavailable.
2. Acquire the sorted complete resource set for an assignment atomically or queue it without partial holds. Keys cover canonical repository/path overlap, shared test/database state, browser context, account capacity and integration writers. Use FIFO eligible admission within priority, round-robin among equal-priority goals at settled boundaries, with waiting-age promotion after each full eligible round. Never preempt a productive owned compiler or hold a scarce worker slot while waiting for another dependency.
3. Persist run priority and queue requests as facts; derive current counts/waits from live leases, not synchronized counters. Restart reconstructs admissions from the full journal and current ownership; it does not launch every formerly active goal at once. Goal dependency completion names the required verified result/version, rather than treating another goal’s badge as sufficient.
4. Show two active goals and a third waiting with the actual limiting resource/dependency. Pause, stop, priority, browser takeover, question and draft actions remain scoped to the selected goal. Switching between projects restores that goal’s conversation/details context and never imports another lead’s private context.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| N10 | Two different Tasks describe similarly worded problems while a start request for one is replayed | Admission resolves ownership | The distinct goals stay distinct with owned artifacts, while the replay attaches to the original run without launching another author. |
| W07 | Two goals have separate source workspaces but share a browser profile or interactive input surface | Both need conflicting UI/account changes | Compatible independent surfaces are proved or the conflicting operations serialize with an explicit wait. Neither goal changes the other's login, target, or test state; shared access is not treated as separate security isolation. |
| V01 | Robin starts two independent goals, in the same or different projects | Work is admitted | Separate task leads receive scoped context and own their plans/workers/results; the user can steer both from the existing cockpit without copying memory or spawning lead instances manually. |
| V02 | Two executing goals each have a lead and worker under default policy | More goals/work become ready | No fifth orchestration-owned runtime or third executing goal is admitted; work queues visibly under shared/per-goal/account/resource limits, with no fabricated count for opaque native children. |
| V03 | A long campaign is active, another goal waits on user input, and a third is ready | A bounded work boundary or supported lead checkpoint is reached | Exact ownership settles as needed, eligible goals receive fair admission, and the ready goal does not wait for the campaign to finish or for unrelated user input. Productive compilers are not killed for rotation. |
| V04 | Two distinct Command goals require an overlapping source/contract or shared-checkout writer | Both become ready | The host coordinates isolation or serialized ownership/integration and preserves valid read-only/other-project progress; independently passing diffs cannot silently overwrite each other. |
| V05 | Goals share a provider account whose known authorized cap is exhausted | Either requests another assignment | All affected spending admission obeys the same cap; another lead does not reset the budget, while permitted independent work and owned cleanup can continue. |
| V06 | Similar questions are pending in different goals | The user answers, pauses, or stops only the selected goal | The action settles only the correctly correlated scope; other goals continue, and an explicit multi-goal instruction is separately applied to its named scope. |
| V07 | One goal integrates a change needed by another goal's active plan/checks | The result is delivered | The dependent lead receives the accepted versioned result, reconciles affected work/evidence, and never treats a stale contract or another lead's permission as current authority. |
| V08 | Multiple active/paused goals and memory/resource publications exist at host interruption | Recovery runs | Each goal restores its own lead authority, memory versions, waits, usage, and ownership; paused work stays paused and duplicate workers/publications are not admitted. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_scheduling.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P13-N10** — `cargo test --locked --test goal_scheduling p13_n10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_n10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-W07** — `cargo test --locked --test goal_scheduling p13_w07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_w07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V01** — `cargo test --locked --test goal_scheduling p13_v01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V02** — `cargo test --locked --test goal_scheduling p13_v02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V03** — `cargo test --locked --test goal_scheduling p13_v03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V04** — `cargo test --locked --test goal_scheduling p13_v04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V05** — `cargo test --locked --test goal_scheduling p13_v05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V06** — `cargo test --locked --test goal_scheduling p13_v06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V07** — `cargo test --locked --test goal_scheduling p13_v07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P13-V08** — `cargo test --locked --test goal_scheduling p13_v08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p13_v08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P13-J1 — Three goals and fairness:** Start two overlapping Command goals and one independent DevManager goal. Observe at most two executing goals/two top-level runtimes each/four total, disjoint eligible progress, exact waits and eventual admission of the third when capacity is available. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P13-J2 — Resource intersections:** Exercise repo-path, browser, shared test database and account contention with opposing requested resource order. Verify no partial-hold deadlock, no overlapping owner and no poisoned unrelated transport; independent read-only work stays eligible. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P13-J3 — Restore and scoped controls:** Restart with multiple queued/running goals and a versioned dependency. Reconstruct one owner per lease and admit within limits. In the native board change priority, pause one goal, answer another and retain three distinct drafts without cross-goal effects. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P13-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-RUN`, `REF-AGENTS`, `REF-RECOVERY`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P13-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G13.** One shared host admission owner, current ResourceRequest/lease intersection, persisted priority/eligible queue lineage and fair default 2-goal/2-runtime-per-goal/4-global limits. The native board and scoped controls show exact waits and independent lead state.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/resource.rs](../../../src/domain/resource.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/host/connection.rs](../../../src/host/connection.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/browser/gateway.rs](../../../src/browser/gateway.rs)
- [src/ui/native_shell.rs](../../../src/ui/native_shell.rs)
- [src/ui/workspace_layout.rs](../../../src/ui/workspace_layout.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Default two executing goals; maximum two top-level runtimes per goal including lead; maximum four globally. Shared stricter limits win. No partial resource set retained while waiting for its remainder.

## Out of scope

Do not time-slice live provider/compile processes by killing them, start another global scheduler service, or make users assign every worker.

## Risks & watch-fors

- **A priority queue starves ordinary goals:** Use persisted eligible-round aging and test eventual admission under a finite stream of higher-priority work; never invent progress while a hard constraint persists.
- **Two resource managers disagree:** All goal effect admission reaches the existing single host authority and shared lease transaction; UI and lead counts are projections only.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02, CONTRACT-G04, CONTRACT-G05, CONTRACT-G06, CONTRACT-G11 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
