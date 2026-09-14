Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Resolve questions and apply current scoped steering

Phase 05 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The lead answers routine worker questions from the agreement, policy and current evidence. Only a consequential unresolved decision reaches the human. Questions, secure access and changes of direction stay scoped to the current goal and exact action. The native composer remains usable while independent work continues.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G05. Route worker questions to the lead with exact logical message, predecessor, recipient, assignment and action identity. Record a lead answer with its source rationale when existing authority settles it. Persist genuine product/risk/cost questions, choices and a recommendation only when supported. A displayed or preselected choice is never a submitted answer.
2. Receive user answers and steering through the same native action path with optimistic Task and goal revisions. Apply once to the current question/action/account/permission version, invalidate stale pending actions, and record supersession visibly. Technical choices continue; WHAT changes use the agreement amendment and complete downstream publication from phase 3. A new stop has priority over routine progress, but cannot undo an already-started external effect.
3. Use the existing secure native sign-in/Secret handoff for passwords, tokens and OTPs. The decision card shows nonsecret account, action, target and expected cost, including paid reads. Recheck account revocation and actual tool access immediately before dispatch and on replies; preparing or previously approving another account is insufficient. Reuse existing consent instead of repeatedly asking for it.
4. Implement sending/accepted/superseded/failed states in the Needs you card. Retain each goal’s draft, selected question, scroll and focus through query failures, task switches and detail overlays. Browser takeover transfers the exact existing lease and resumes only after the matching user handback. Unrelated work remains eligible while the blocked decision waits.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| H07 | Work is running and a pending action uses an earlier constraint | Robin asks for status, then narrows that constraint | Status is answered without restarting the goal; the correction updates the same run and prevents obsolete queued actions while already-started effects settle truthfully. |
| H10 | Two workers need the same permitted account/environment and its login has expired | The lead prepares required verification | One scoped native login/secret handoff is surfaced, unrelated work continues, and no secret enters model context or evidence. A correlated completion resumes dependents only after the intended account/access is verified; a wrong account remains unresolved. |
| N04 | “This page” cannot be resolved from the current task, available application, or supplied material | Entry investigates the target | The lead asks for the missing page/context once, preserves useful independent preparation, and does not invent the target. |
| Q01 | A worker asks for the test command already verified in the Context Pack | Its question reaches the coordinator | The lead answers from the source and continues without asking Robin. |
| Q02 | An agreed behavior cannot be satisfied without changing a tenant-isolation invariant | The lead confirms the conflict | One precise user decision stays pending; independent work continues; silence is not taken as approval. |
| Q03 | Robin approves a scoped amendment affecting two phase docs and TEST-PLAN | The lead applies the answer | SPEC/amendment history and every affected artifact/rework entry are reconciled before dependent execution resumes; the answer survives a session replacement. |
| Q04 | A question was superseded and two clients still display an earlier version | Answers arrive concurrently or after the change | Only a valid current decision settles; obsolete answers cannot authorize new work, and clients show the actual settled answer without duplicate questions. |
| Q06 | A connected tool action is preparing or awaiting an answer, including a paid read; the account is then disconnected, replaced or restricted | A stale answer or credential-preparation callback arrives; repeat after dispatch and with a failed replacement verification | Recheck the exact account/action/target, current connection/authority and applicable cost before dispatch. Obsolete prepared work cannot execute or gain permission on another account. A failed replacement leaves an undisconnected valid connection intact; an already-dispatched effect is reconciled without fictitious rollback or blind retry. Existing authority avoids a repeated ask and independent work continues. |
| W08 | An agent has queued browser input and Robin takes over that exact surface for a sensitive step | Control is later returned after the page/account changes | Obsolete input is fenced, other eligible work continues, and resumption rechecks current ownership, page, and account. A late response cannot act on the replacement target or turn takeover into broader authorization. |
| U02 | A product question blocks one branch of work | The user reconnects | The same scoped decision remains in Needs you, alongside truthful progress for independent work. |
| U04 | Workers emit repeated updates and equivalent questions, then a verified result or genuine user-only access need appears | Robin views, dismisses, or marks activity read | The lead presents concise progress and one scoped action need, preserving detailed evidence. Read/unread attention stays distinct from execution and decision state; dismissing a notice does not approve or stop work. |
| U15 | Plan, Agents, Checks, or Knowledge is open for one goal with a draft and pending decision | The user switches goals, opens an artifact/worker, or closes details at a narrow size | Details, decisions and actions remain scoped to the selected goal; drafts and return context are preserved, stale outcomes cannot affect its replacement, and focus returns to a usable native control. |
| U20 | A native goal encounters an access-mode refusal, revoked prepared action, incomplete scanner and a finding derived from external content | The user inspects Agents, Checks, Knowledge and any actually required decision at normal and narrow geometry | Actual access/recovery, covered/skipped/error state and originating evidence are visible through existing details and safe viewers. A required decision names the non-secret account/action/target and known cost; already-authorized work adds no approval step. Draft, task identity and focus/return survive reconciliation. Real host wiring, native journeys and matched appearance evidence cover these states. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_decisions.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P05-H07** — `cargo test --locked --test goal_decisions p05_h07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_h07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-H10** — `cargo test --locked --test goal_decisions p05_h10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_h10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-N04** — `cargo test --locked --test goal_decisions p05_n04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_n04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-Q01** — `cargo test --locked --test goal_decisions p05_q01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_q01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-Q02** — `cargo test --locked --test goal_decisions p05_q02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_q02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-Q03** — `cargo test --locked --test goal_decisions p05_q03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_q03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-Q04** — `cargo test --locked --test goal_decisions p05_q04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_q04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-Q06** — `cargo test --locked --test goal_decisions p05_q06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_q06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-W08** — `cargo test --locked --test goal_decisions p05_w08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_w08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-U02** — `cargo test --locked --test goal_decisions p05_u02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_u02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-U04** — `cargo test --locked --test goal_decisions p05_u04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_u04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-U15** — `cargo test --locked --test goal_decisions p05_u15 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_u15`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P05-U20** — `cargo test --locked --test goal_decisions p05_u20 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p05_u20`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P05-J1 — Lead answers and real escalation:** Ask one question answered in the SPEC and one unresolved retention question. Only the latter reaches Needs you; another independent item progresses. Submit an answer, reconnect before its receipt and verify exactly one effective decision. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P05-J2 — Stale authority:** Change goal scope and revoke a prepared connection before answering the original card. Verify the old answer is superseded and cannot authorize the new action/account; existing unaffected authority stays usable without another ask. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P05-J3 — Native input and secure handoff:** At desktop and narrow geometry switch goals with unsent text, open/close details with keyboard focus return, submit the current decision, and use the native Secret flow. Verify no secret appears in the conversation/evidence and the scoped browser lease resumes correctly. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P05-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-DECISION`, `REF-NARROW`, `REF-STEERING`, `REF-EFFECTIVE-ACCESS`, `REF-ACCESS-NARROW`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P05-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G05.** DecisionRecord and correlated message/answer/restore actions, prepared-effect account/authority validation and existing secure browser handoff. Native Needs you/composer retains scoped sending/superseded/draft/focus state.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/provider_input.rs](../../../src/domain/provider_input.rs)
- [src/providers/input.rs](../../../src/providers/input.rs)
- [src/browser/pane.rs](../../../src/browser/pane.rs)
- [src/browser/gateway.rs](../../../src/browser/gateway.rs)
- [src/ui/task_cockpit/composer.rs](../../../src/ui/task_cockpit/composer.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)
- [src/client/action.rs](../../../src/client/action.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

One actionable human question at a time per goal; independent work can continue. A message retry reuses its logical ID; a new identical message has a new ID. No secret bytes in tool arguments, model context or visible approval summaries.

## Out of scope

Do not add an approval for already authorized reversible work, change a universal policy from goal steering, or block all goals on one answer.

## Risks & watch-fors

- **A late callback revives old authority:** Validate current run, action epoch, question version, account revision and recipient on both receipt and application.
- **UI optimism grants permission:** Disable dependent dispatch until the accepted current host fact, while preserving an honest Sending state and draft on failure.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02, CONTRACT-G03 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
