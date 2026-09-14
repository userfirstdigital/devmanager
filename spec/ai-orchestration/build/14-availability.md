Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Recover from profile limits without changing authority

Phase 14 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A goal can wait or select another permitted capable profile when its current one is unavailable. Shared availability observations remain scoped and fresh. Retry, budget and authentication choices stay continuous across replacements. Agents explains what is known, unknown and eligible next.

## Preconditions

- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) exposes **CONTRACT-G06**. Run `cargo test --locked --test goal_evidence -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [13 — Run multiple task leads under fair shared ownership](13-scheduling.md) exposes **CONTRACT-G13**. Run `cargo test --locked --test goal_scheduling -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G14. Record provider/model/account/tool availability observations with source, observation time, scope, expiry or reported reset and confidence. Eligibility intersects every applicable restriction; a narrower expired success cannot clear a wider current denial. Coalesce equivalent recovery probes through a single host-owned lease while notifying all affected goals of the resulting fact.
2. Filter candidate profiles by hard capability, actual effective access, project/role policy, auth mode, budget and tool/environment requirements before ranking. Preserve working conversation/profile affinity; fall back only when needed and qualified. A different account, paid API route or broadened permission needs existing applicable authority or a real decision, never a silent substitute. A stopped conversation with no exact-resume support becomes an explicitly separate attempt.
3. Share the original assignment’s unchanged host retry allowance across adapters, wake timers and fallback. Native harness retries remain owned by the native runtime and are reported as opaque when unobservable; do not layer another loop while it is active. Carry cumulative cost/time observations and cause repair counts through every replacement. Unknown spend is unknown, not zero or fabricated token savings.
4. Agents shows selected/actual profile, eligibility or refusal reason, observed reset versus local recheck, freshness, scoped budget and queued recovery. An expiring restriction triggers an actionable wake only when the new intersection is eligible. Keep unrelated profiles/goals moving.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| R03 | One goal's default two-runtime allowance is occupied by its lead and worker | Another assignment for that goal becomes ready | It queues under both per-goal and shared admission; native-child usage is not falsely counted as fully known. |
| R04 | A worker demonstrably lacks a needed capability and a permitted eligible alternative exists | The lead diagnoses the failure | It settles the old writer, hands off the preserved facts/work to the alternative, and continues without a provider-picker question or resetting the finding's fix-round count. |
| R07 | A previously qualified provider executable or required tool changes | Another assignment is ready | Affected capabilities are rechecked before admission, with an eligible permitted fallback if needed; old qualification is not silently reused. |
| V10 | Two goals share a proven account with an active account-wide restriction and an expired model-specific restriction | Selection runs and an unrelated scope reports success | The wider restriction still governs both goals; success cannot clear it without resolving evidence. A local earlier recheck is not presented as the provider's reset, and unaffected eligible work continues. |
| V11 | Several goals await the same supported provider recovery while an older readiness probe remains outstanding | Retry eligibility arrives, one goal is stopped, and a newer restriction is observed | Equivalent current probes/wakeups coalesce under shared admission; stale results cannot clear the newer restriction or restart the stopped goal. Eligible goals resume fairly without a burst of duplicate recovery requests. |
| D06 | A hard known budget is exhausted or a required provider is rate-limited | More work is ready | The host admits only work allowed by the remaining authority/capacity; the specific condition is visible and unknown quota is not fabricated. |
| D18 | A run changes lead/provider after consuming part of its known budget | More work is requested near the limit | Total consumed usage and retry/repair history remain attributed to the run; unauthorized admission is refused while owned cleanup can settle. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_availability.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P14-R03** — `cargo test --locked --test goal_availability p14_r03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_r03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P14-R04** — `cargo test --locked --test goal_availability p14_r04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_r04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P14-R07** — `cargo test --locked --test goal_availability p14_r07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_r07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P14-V10** — `cargo test --locked --test goal_availability p14_v10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_v10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P14-V11** — `cargo test --locked --test goal_availability p14_v11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_v11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P14-D06** — `cargo test --locked --test goal_availability p14_d06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_d06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P14-D18** — `cargo test --locked --test goal_availability p14_d18 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p14_d18`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P14-J1 — Qualified fallback:** Deny one model capability and rate-limit the active permitted profile. Verify incompatible candidates are excluded, current native retries retain ownership, and only an already authorized qualified replacement starts after settlement. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P14-J2 — Shared restriction truth:** Apply overlapping account/model restrictions with different expiry and a stale success. Three affected goals produce one eligible recovery probe; the wider current denial remains until its own valid evidence clears it. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P14-J3 — Budget and native waits:** Exhaust the shared two-retry allowance across timer/adapter/fallback paths and change provider twice. Counts/cost observations remain continuous, unknown usage stays unknown, and native Agents shows the exact limit and next permitted action. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P14-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-AGENTS`, `REF-RECOVERY`, `REF-EFFECTIVE-ACCESS`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P14-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G14.** Scoped Availability/UsageObservation/BudgetPolicy records, hard-capability and effective-access filtering, profile affinity, coalesced recovery and one composed host-retry allowance. Agents shows current limits, unknown observations and authorized fallback.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/providers/capabilities.rs](../../../src/providers/capabilities.rs)
- [src/providers/adapter.rs](../../../src/providers/adapter.rs)
- [src/providers/orchestrator.rs](../../../src/providers/orchestrator.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/ui/provider_settings.rs](../../../src/ui/provider_settings.rs)
- [src/ui/task_cockpit/config_sidebar.rs](../../../src/ui/task_cockpit/config_sidebar.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

At most two automatic retries for the same unchanged host assignment across all host layers. Repair attempts retain the phase-7 four-completed-repair limit. No stale narrower observation clears a wider active restriction.

## Out of scope

Do not add a credential router, switch a subscription to paid API use implicitly, or claim total spend from partial provider observations.

## Risks & watch-fors

- **Automatic fallback silently changes cost:** Treat account/auth/billing mode as hard eligibility and bind any needed decision to its exact current target and cost.
- **Repeated probes multiply traffic:** Use one scoped observation/recovery lease and a durable next-eligible wake rather than one timer per lead/provider layer.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G02, CONTRACT-G05, CONTRACT-G06, CONTRACT-G13 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
