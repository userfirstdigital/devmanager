Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Close audit gaps with bounded independent diagnosis

Phase 07 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The lead fixes audit and verification gaps without requiring the user to coordinate repeated sessions. Repeated failures trigger a different diagnosis rather than another identical attempt. Productive owned work keeps running even when quiet. The native account shows remaining causes, attempts and the precise blocker when the finite recovery allowance is exhausted.

## Preconditions

- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G07. Group findings by evidence-backed root cause, keeping each original finding identity and scope. Dispatch the smallest permitted fix, run its acceptance, and return to a fresh verifier for closure. A fixer records what changed and evidence but cannot tick its own audit item. Disputes distinguish incorrect facts, misread requirements and a proposed better product divergence; only the last needs a genuine outcome decision.
2. After two completed failed repair attempts for one cause, dispatch a fresh independent diagnosis with original evidence, prior hypotheses and counterexamples. Permit at most two further materially different repairs supported by that diagnosis. Keep the four-completed-repair ceiling across renamed groups, provider replacement, resumed sessions and merged causes. A crashed/incomplete verification is a recovery event, not a completed failed repair and not a pass.
3. Track process ownership and verified progress while waiting for compilers/tests/wrappers. Use exact process handles and current activity; a 30-minute progress report or 60-minute reassessment cannot kill a productive compiler. At actual unsafe overlap/stall, settle or terminate only the owned tree and preserve its diff before a replacement. Let independent unaffected items continue while one cause waits.
4. Checks displays original cause membership, each hypothesis/change/result, the independent diagnosis and remaining allowance. A precise blocked item remains on the board and in the canonical tracker; no repeated generic “still working” loop or hidden reset of attempt history.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| A05 | `P2-B1` remains unresolved after two ordinary repair attempts | The lead selects the next action | It arranges independent diagnosis and a permitted materially different repair within the recovery limit; no automatic question to Robin is triggered merely by the count of two. |
| A06 | The default four completed repair attempts for the same finding have been consumed and it remains open | Another repair would be admitted | The affected work becomes a precise blocker with diagnosis and evidence; independent work continues and no renamed finding or provider switch evades the limit. |
| A07 | Verification of a submitted repair crashed before producing an acceptance result, then the lead restarts | Recovery resumes | The attempt remains unverified, its writer is not duplicated, and bounded environment recovery resumes the check; consumed attempt/usage history is preserved. |
| D12 | A worker process exits unexpectedly while its assignment still appears active | Host reconciliation observes the exact exit | The run diagnoses and recovers/reassigns as permitted, or exposes the precise unrecoverable condition; it does not remain indefinitely Running without an owner. |
| D13 | A compiler is still making progress in the assignment's owned process tree despite no new chat output | The supervisor checks for a stall | It preserves the productive tree, avoids a duplicate build, and reports the actual wait instead of timing out the work solely for silence. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_repairs.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P07-A05** — `cargo test --locked --test goal_repairs p07_a05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p07_a05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P07-A06** — `cargo test --locked --test goal_repairs p07_a06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p07_a06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P07-A07** — `cargo test --locked --test goal_repairs p07_a07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p07_a07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P07-D12** — `cargo test --locked --test goal_repairs p07_d12 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p07_d12`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P07-D13** — `cargo test --locked --test goal_repairs p07_d13 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p07_d13`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P07-J1 — Bounded cause loop:** Force two completed unsuccessful repairs, then provide a new independent diagnosis and two different unsuccessful repairs. Verify exactly four completed repairs, no fifth dispatch, stable finding/cause lineage and independent work continuing. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P07-J2 — Incomplete check and productive wait:** Crash the verifier before its terminal summary and separately keep a compiler alive with verified file/process activity beyond the progress checkpoint. The first remains incomplete with retained repair counts; the second retains its single writer and never starts a duplicate. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P07-J3 — Dispute and native history:** Prove one audit fact incorrect from code/output and present a second deliberate requirement divergence. Independently close the factual dispute; route only the WHAT change to a current decision. Open the complete attempt history and preserved diff in native Checks. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P07-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-TESTS`, `REF-RECOVERY`, `REF-UNCERTAIN-EFFECT`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P07-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G07.** RepairAttempt with original cause lineage, 2+independent-diagnosis+2 completed-repair admission, fresh closure/dispute and exact productive-process ownership. Checks exposes preserved hypotheses, attempts and finite recovery disposition.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/operation.rs](../../../src/domain/operation.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)
- [src/host/diag.rs](../../../src/host/diag.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Two completed ordinary repairs, one independent diagnosis, then at most two materially different completed repairs per root cause. Incomplete verification consumes no completed repair but remains subject to transport/recovery and budget limits.

## Out of scope

Do not weaken tests to obtain green, silently accept a product divergence, or make every repeated failure an automatic human handoff.

## Risks & watch-fors

- **Cause regrouping evades the repair ceiling:** Maintain transitive original incident/attempt membership; merging causes preserves the union of completed attempts and never grants a fresh allowance.
- **Wrapper timeout hides a live worker:** Inspect exact wrapper descendants and target ownership before any retry; do not infer exit from a chat response or shell wrapper timeout.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G02, CONTRACT-G03, CONTRACT-G04, CONTRACT-G06 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
