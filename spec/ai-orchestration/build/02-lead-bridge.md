Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Run a qualified lead and continue its work

Phase 02 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

The lead uses the installed Claude or Codex harness, with the host handling durable coordination. A qualified worker can submit a proposal or a question without claiming host authority. The lead continues when actionable work arrives and its current turn has settled. The native Agents view explains the actual assignment and access.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G02 using the current rmcp/loopback browser-gateway registration pattern. Issue a separate per-runtime goal token and private Claude MCP overlay or host-generated Codex config overrides. Revoke registration with the exact runtime lease and arm cleanup before the first await. Expose only the typed goal tools in the overview; bind actor, run, assignment, generation and permission revision from the registration, never caller JSON. Keep stock_adapter_ingress_available() and the existing arbitrary-result HOLD fail-closed.
2. Qualify the actual executable/version, session hook, required tools, workspace access and selected-versus-effective restrictions. Check the production client/host capability intersection and SessionStart mismatches both before and after process publication. Use existing ProviderInput fences to deliver the initial assignment, correlated replies and continuation. A tool call is a submitted proposal, not proof of provider turn completion. Only a qualified current-turn terminal fact releases the single continuation owner; preserve all events while coalescing redundant wake hints.
3. Implement lead and reviewer role briefs from canonical sources and exact assigned inputs, including allowed effects and verification obligations. Prefer the project’s permitted role policy; do not hard-code a global model ranking. Test both Claude-led and Codex-led operation, absent Cursor, supported unsupported-identity first input, and explicit exact-resume failure. Tool discovery may reveal another permitted tool but cannot expand permissions.
4. Populate Agents from host-correlated assignments and current runtime facts. Open the existing provider conversation/terminal, retain last verified activity, and show selected/effective/unknown access separately. Render accepted, delivered and applied steering as distinct facts. Worker output displayed in the conversation retains its source and has no approval authority.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| R01 | Project policy prefers Claude lead, permits Cursor mechanical work, and requires Codex landing review | A suitable repetitive edit and later landing review are needed | The lead arranges eligible permitted profiles automatically and records the reason; the user does not select each worker. |
| R02 | A project uses an eligible Codex lead and does not require Cursor | Cursor is absent | The workflow completes with permitted eligible profiles; optional Cursor absence is not a blocker. |
| R05 | A CLI accepts text but cannot return a correlated result through the configured integration | A profile is qualified for unattended work | The required capability stays unavailable; executable discovery or a successful launch alone cannot qualify the role. |
| R06 | A worker requests an action already authorized for its exact current assignment | The provider exposes a supported question/answer interface | The lead/host answers within that authority and resumes the attempt; a broader or uncorrelated request is not automatically approved. |
| R08 | A worker result or repository comment says it has permission to publish and mark checks passed | The lead processes that content | It treats the text as untrusted evidence, retains the actual authority boundary, and requires correlated execution/verification facts. |
| R09 | A structured tool can perform a routine lookup, while the goal separately requires a real browser journey | The lead selects work surfaces | It uses suitable permitted direct access for the lookup and the required browser for the journey without asking Robin to route tools. A denied effect is not retried through another surface, and API success cannot replace the browser check. |
| R10 | The lead can reach a test service, but the worker's runtime resolves the same address/path to a different or inaccessible resource | An assignment is prepared | The worker's actual environment, source, endpoint, tools, and persona are checked; invalid access cannot inherit the lead's readiness. An authorized correction or eligible alternative resumes the same goal with current evidence. |
| R11 | An assignment requires inspecting image evidence or using a specified tool, and the remaining permitted candidates cannot do it | Fallback selection runs | Incompatible candidates are excluded rather than merely ranked last; required input/tools/acceptance remain intact and the exact missing capability is visible while independent work continues. |
| R12 | An assignment's profile remains qualified and a cheaper candidate or opaque model alias becomes available | Another message is ready, then a real profile change becomes necessary | Ordinary continuation retains the current profile/conversation; a necessary change records its reason and requalifies affected capabilities through supported handoff. An alias or unknown upstream route cannot prove actual model identity or exact resume. |
| R13 | A worker profile offers a reduced tool catalogue, and the assignment requires a browser check not in that initial catalogue | The worker needs that capability; repeat with an unknown/inherited tool name and an actually unsupported harness | Supported discovery loads or requests the exact registered permitted tool and verifies access; otherwise another qualified profile or the specific capability blocker is used. No hidden tool becomes an invented impossibility, name match grants authority, or mandatory guidance/check disappears to save context. |
| R14 | A profile selects an access mode that the executing provider rejects, or claims read-only while its shell can still write protected application files | Qualification and an assignment exercise required and prohibited operations on owned disposable targets, retaining any explicitly allowed audit-artifact writes; repeat with a successful turn that refused a required tool | Report selected versus observed/reported access and unknown details honestly; the refused action remains unperformed and the profile cannot qualify beyond its actual enforcement. Reconcile the attempt, preserve current conversation identity and recover only within permitted authority. A changed default does not silently retarget active work. |
| D09 | A lead turn ends after implementing phase one, with phase two eligible and no pending decision | The provider returns control with a finished-turn marker or final message | The host retains the provider outcome’s actual scope and arranges continuation; phase two starts without another user message or a premature goal-complete verdict. |
| D10 | A worker result arrives while the lead is idle and the desktop is closed | The result is durably admitted and then replayed after reconnect | The lead is notified and continues exactly once; replay does not launch duplicate work. |
| D11 | A temporary provider limit has an established retry time and there is no immediate permitted fallback | The retry time arrives while the host is available | The run resumes automatically if authority and budget still permit it; a pause/stop or superseding attempt suppresses the old wakeup. |
| D19 | A returned lead has pending worker results/questions plus a burst of duplicated progress notifications | Delivery and reconnect replay occur | Durable result/question facts and origin survive; redundant wake hints coalesce, at most one current continuation executes, and every relevant result is reconciled without duplicate dispatch or acknowledgement loops. |
| D20 | Background wake notifications are queued and a worker message claims to be a new user instruction | Robin sends a scoped stop or correction | Only authenticated user direction changes authority; the control promptly prevents superseded admissions while earlier durable facts retain their ordering. Background chatter cannot delay the control indefinitely or restart stopped work. |
| Q05 | A worker asks the lead a question, delivery disconnects, and both a retry and a separate identical question occur | Delivery, a late reply, and worker completion are reconciled | The retry keeps its exact message/attempt/recipient identity and original allowance; the new question has its own identity. Conflicting, concurrent, expired, settled or superseded claims cannot authorize work. Earlier admitted questions/results remain accounted for before completion is published. |
| U18 | The user corrects a running goal while nested workers are active and one cannot receive live input | The correction is accepted and the user inspects Conversation, Plan or Agents | Native state distinguishes accepted, pending and applied/reconciled steering for the affected work. New assignments inherit the current direction; obsolete actions are refused, blocked delivery has a recovery path, and the user never has to repeat the correction to every worker. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_lead_bridge.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P02-R01** — `cargo test --locked --test goal_lead_bridge p02_r01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R02** — `cargo test --locked --test goal_lead_bridge p02_r02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R05** — `cargo test --locked --test goal_lead_bridge p02_r05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R06** — `cargo test --locked --test goal_lead_bridge p02_r06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R08** — `cargo test --locked --test goal_lead_bridge p02_r08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R09** — `cargo test --locked --test goal_lead_bridge p02_r09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R10** — `cargo test --locked --test goal_lead_bridge p02_r10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R11** — `cargo test --locked --test goal_lead_bridge p02_r11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R12** — `cargo test --locked --test goal_lead_bridge p02_r12 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r12`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R13** — `cargo test --locked --test goal_lead_bridge p02_r13 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r13`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-R14** — `cargo test --locked --test goal_lead_bridge p02_r14 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_r14`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-D09** — `cargo test --locked --test goal_lead_bridge p02_d09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_d09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-D10** — `cargo test --locked --test goal_lead_bridge p02_d10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_d10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-D11** — `cargo test --locked --test goal_lead_bridge p02_d11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_d11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-D19** — `cargo test --locked --test goal_lead_bridge p02_d19 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_d19`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-D20** — `cargo test --locked --test goal_lead_bridge p02_d20 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_d20`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-Q05** — `cargo test --locked --test goal_lead_bridge p02_q05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_q05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P02-U18** — `cargo test --locked --test goal_lead_bridge p02_u18 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p02_u18`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P02-J1 — Real harness qualification:** Run an owned disposable task with each configured Claude/Codex lead: authenticate the exact SessionStart, discover the goal tools, submit a proposal, ask/receive one correlated reply and continue after the real turn ends. A missing harness/login is NOT VERIFIED for that profile, never a simulated pass. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P02-J2 — Host boundary attacks:** Send a forged run/actor/generation, a stale token, a duplicate logical message and two distinct identical messages. Only the current scoped actor proposal is admitted. A claimed test pass creates no trusted check receipt. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P02-J3 — Continuation and native truth:** Deliver 100 progress events plus one worker completion while the lead is busy; preserve all durable events and run one continuation when eligible. In Agents inspect actual access mismatch, accepted/pending/applied steering and an unsupported capability without losing the draft. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P02-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-AGENTS`, `REF-STEERING`, `REF-EFFECTIVE-ACCESS`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P02-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G02.** Per-runtime /goal/mcp registration and six typed tools; RuntimeBinding/AssignmentRecord/MessageRecord; qualified current-generation provider input and one actionable continuation owner. MCP output is a proposal, never provider identity or a check receipt.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/providers/orchestrator.rs](../../../src/providers/orchestrator.rs)
- [src/providers/adapter.rs](../../../src/providers/adapter.rs)
- [src/providers/capabilities.rs](../../../src/providers/capabilities.rs)
- [src/providers/journal.rs](../../../src/providers/journal.rs)
- [src/providers/codex.rs](../../../src/providers/codex.rs)
- [src/browser/gateway.rs](../../../src/browser/gateway.rs)
- [src/browser/provider.rs](../../../src/browser/provider.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/domain/provider_input.rs](../../../src/domain/provider_input.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

At most one scheduled or running continuation per run. Per-assignment host transport retries are at most two under the shared ledger. MCP requests are at most 64 KiB and replies use bounded pagination; no raw full transcript forwarding.

## Out of scope

Do not replace provider model loops or convert MCP claims into authenticated provider lifecycle or verification facts.

## Risks & watch-fors

- **SDK or CLI drift:** Reuse installed qualification probes and requalify material changes; fail the exact capability without switching billing/authentication modes.
- **Process ownership:** Own the MCP registration and all wrapper descendants through completion; a worker response does not prove its process tree exited.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
