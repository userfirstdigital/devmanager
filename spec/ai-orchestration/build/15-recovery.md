Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Resume exact work after interruption and reconcile effects

Phase 15 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A closed window or dead session does not erase the goal. Recovery resumes from durable invocation and effect lineage while retaining valid work and current decisions. Unknown external outcomes remain explicit until safely reconciled. Pause and stop complete only when the exact owned effects and processes have settled.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) exposes **CONTRACT-G07**. Run `cargo test --locked --test goal_repairs -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [11 — Use canonical project and universal memory with scoped context](11-memory.md) exposes **CONTRACT-G11**. Run `cargo test --locked --test goal_memory -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [12 — Sharpen rules through coherent evidence-based revisions](12-learning.md) exposes **CONTRACT-G12**. Run `cargo test --locked --test goal_learning -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [13 — Run multiple task leads under fair shared ownership](13-scheduling.md) exposes **CONTRACT-G13**. Run `cargo test --locked --test goal_scheduling -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [14 — Recover from profile limits without changing authority](14-availability.md) exposes **CONTRACT-G14**. Run `cargo test --locked --test goal_availability -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G15 and complete the fault matrix across every preceding slice. Retain root invocation, parent invocation, occurrence ID, method/version, agreement/plan version, checkpoint predecessor and exact operation/effect identity. Rebuild from the complete task-scoped event stream, including delivery and operation-terminal facts. Each fixed journal is an exact ordered prefix; holes, duplicates, foreign lineage and out-of-order entries fail closed before effects or cleanup counts are produced.
2. At host restart, reconcile owned process and provider records, registrations, pending publication plans, outbox residues and external effect receipts. Distinguish exact-resume from a new replacement attempt; never synthesize provider IDs from cwd/transcript order. If an effect may have completed but checkpoint persistence failed, use its destination-specific idempotency/read-back contract. Without a safe reconciliation result it stays uncertain and cannot be blindly retried.
3. Pause prevents new dispatch immediately and shows Pausing while already admitted effects settle; Paused retains durable work. Stop increments the action epoch, cancels only queued work, joins or deliberately stops exact owned process trees, clears their registrations/leases and shows Stopping until every required cleanup receipt exists. Window close detaches the client and does not imply Stop. User stop wins a race with late lead suggestions, while successful in-flight physical writes still advance delivery cursors.
4. Use supported native context/checkpoint recovery or a fresh bounded canonical handoff while preserving current steering, pending question identity and one continuation owner. Storage failure during publication retains the staged source and journal for reconciliation; no completion is reported before the commit/receipt lineage is durable. Native recovery remains in the same goal, with preserved edits, exact action uncertainty, retry/budget history and a current reconnect path.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| N11 | A lead is replaced during a project investigation or test campaign | The replacement reconstructs the run | Original outcomes, milestone dependencies, baselines, rejected hypotheses, cause links, consumed attempts, and next eligible work survive; valid observations and workers are not blindly repeated. |
| D01 | The lead's context is lost while a worker wrapper is still running | Recovery starts | The lead reconstructs artifacts/ownership and joins or stops that exact worker before retry; no duplicate writer is launched. |
| D02 | The desktop closes during implementation | It is reopened while the host remains live | Work continued under the host and the current run/decisions reappear without recreating workers. |
| D03 | The host restarts and exact provider resume fails | Recovery runs | The failure is visible; no silent fresh conversation is substituted; preserved work supports an explicitly identified replacement assignment if authorized. |
| D04 | A permitted external effect occurred but its acknowledgement was lost | Retry becomes eligible | The effect is reconciled before retry and is not performed twice. |
| D05 | A run owns a worker and an unrelated user server is running | Robin stops the run | Only the run's exact owned work is cancelled/settled; edits and evidence remain; the unrelated server survives. |
| D07 | A run has an in-flight owned effect | Robin pauses it | New work stops being admitted, Pausing remains visible until the effect settles, and only then does the run show Paused; resumption reconciles work before continuing. |
| D08 | An active assignment is based on version A of the SPEC | The on-disk SPEC changes to version B | The change is reconciled before affected work advances; version-A evidence is not silently treated as acceptance of version B. |
| D14 | A durable worker-result callback is pending | Robin stops the run before the callback is delivered | The callback cannot restart the stopped run or admit another assignment. |
| D15 | The lead is replaced while a host-owned worker remains valid, then the old lead reconnects | Recovery transfers authority | One current lead may admit work; the old lead is refused, the valid worker is reconciled without duplicate launch, and the new lead resumes from durable facts. |
| D16 | Storage fails after a document write but before its receipt, or midway through an amendment | Recovery runs | File content and durable lineage are reconciled before dependent work; affected mutations stop while intent/evidence cannot be recorded, and no torn artifact set is accepted as complete. |
| D17 | The host becomes available after restart while one run was active and another was paused | No conversation is opened | Eligible active work is reconsidered automatically; the paused run stays paused and no OS-off execution is claimed. |
| D21 | A transient assignment has consumed its host retry allowance and another candidate/reconnect layer can retry | Recovery evaluates the same interrupted work | The layers share the consumed history and applicable bound; no layer grants a fresh allowance. Opaque native retries remain labeled unknown and cannot run concurrently with a competing host redispatch. |
| D22 | A provider has accepted input or emitted partial output when a transport wait fails, and a retry is queued | The user stops the goal | Queued retries are suppressed; delivery/effects and the exact owned activity are reconciled and settled. Transport success or an ignored late response cannot establish completion, clean shutdown, or permission to repeat effects. |
| D23 | A lead hits a supported context limit or its host handoff exceeds capacity while a worker is active and a current user correction changes applicable guidance | Recovery is attempted | The qualified context/checkpoint path restores current mandatory sources, pending work, identities, and accounting from canonical state within existing bounds. Correlated tool pairs remain valid, the complete journal remains intact, and the worker gains no competing writer. A recoverable case continues; an unsupported mandatory-context path exposes its exact limitation without claiming the policy loaded. |
| D24 | Nested work contains repeated identical calls and some completed checkpoints; the lead restarts after a method/plan version changes | Recovery evaluates the pending invocation | Root/parent/occurrence, current assigned versions, inputs and full checkpoint lineage are reconciled. Intentional repeated calls stay distinct; completed valid work is reused, while changed definitions, missing/duplicate checkpoints or invalid parent edges cannot trigger an unsafe cache hit or blind replay. |
| D25 | An owned external action succeeds but execution stops before its result checkpoint, or a handled failure is replayed | Recovery reaches the action again | Missing local output does not authorize repetition. Destination evidence and the current replay policy determine reconciliation/retry; unknown effects remain explicit, failed results remain failed acceptance, and unrelated eligible work can continue. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_recovery.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P15-N11** — `cargo test --locked --test goal_recovery p15_n11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_n11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D01** — `cargo test --locked --test goal_recovery p15_d01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D02** — `cargo test --locked --test goal_recovery p15_d02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D03** — `cargo test --locked --test goal_recovery p15_d03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D04** — `cargo test --locked --test goal_recovery p15_d04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D05** — `cargo test --locked --test goal_recovery p15_d05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D07** — `cargo test --locked --test goal_recovery p15_d07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D08** — `cargo test --locked --test goal_recovery p15_d08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D14** — `cargo test --locked --test goal_recovery p15_d14 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d14`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D15** — `cargo test --locked --test goal_recovery p15_d15 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d15`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D16** — `cargo test --locked --test goal_recovery p15_d16 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d16`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D17** — `cargo test --locked --test goal_recovery p15_d17 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d17`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D21** — `cargo test --locked --test goal_recovery p15_d21 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d21`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D22** — `cargo test --locked --test goal_recovery p15_d22 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d22`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D23** — `cargo test --locked --test goal_recovery p15_d23 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d23`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D24** — `cargo test --locked --test goal_recovery p15_d24 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d24`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P15-D25** — `cargo test --locked --test goal_recovery p15_d25 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p15_d25`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P15-J1 — Exact replay and corruption:** Rebuild after two provider inputs separated by delivery, nested repeated invocation occurrences and a later cleanup snapshot. Compare live/rebuilt projections. Inject holes, duplicate steps, foreign lineage and reordered facts; all refuse before another effect or cleanup progress. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P15-J2 — Effect/checkpoint split:** Complete a disposable external effect and interrupt before checkpoint persistence. Resume with supported idempotent read-back and verify one effect; repeat with an unqueryable target and verify visible uncertainty without resubmission. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P15-J3 — Native stop/restart lifecycle:** While a productive worker and queued dispatch exist, close/reopen the window, pause/resume, then race Stop with a late callback and restart the host. Capture Pausing/Paused/Stopping/Stopped, preserved draft/evidence and exact process/registration/lease cleanup. No unrelated process changes. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P15-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-RECOVERY`, `REF-UNCERTAIN-EFFECT`, `REF-STEERING`, `REF-CONTEXT-DRIFT`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P15-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G15.** Complete-stream invocation/operation replay, exact-prefix publication/effect recovery, uncertainty reconciliation and pause/stop settlement. Native recovery retains current decisions/drafts/work and never substitutes a fresh provider identity for exact resume.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/kernel/replay.rs](../../../src/kernel/replay.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/domain/event.rs](../../../src/domain/event.rs)
- [src/domain/provider_input.rs](../../../src/domain/provider_input.rs)
- [src/host/connection.rs](../../../src/host/connection.rs)
- [src/providers/journal.rs](../../../src/providers/journal.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/ui/native_shell.rs](../../../src/ui/native_shell.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

One exact invocation occurrence and one continuation owner per active run path. Fixed journals are exact prefixes. Stop is settled only after all scoped process, registration, resource and outbox obligations settle; unrelated tasks stay untouched.

## Out of scope

Do not restart unknown effects blindly, infer a provider conversation from timestamps, kill unrelated compilers or weaken corruption checks to make recovery appear successful.

## Risks & watch-fors

- **Projection rows are mistaken for resume cursors:** Validate each row's exact durable event lineage in the same transaction and replay every relevant event before accepting a later fence.
- **Cancellation hides actual delivery:** Keep physical writer high-water and admission permits until write/flush settles; invalidate queued generations, not an already delivered fact.
- **Self-owned cleanup loses its owner:** Arm RAII before ownership transfers and retain/join exact descendants across cancellation; incomplete cleanup remains an execution blocker.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02, CONTRACT-G03, CONTRACT-G04, CONTRACT-G05, CONTRACT-G06, CONTRACT-G07, CONTRACT-G11, CONTRACT-G12, CONTRACT-G13, CONTRACT-G14 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
