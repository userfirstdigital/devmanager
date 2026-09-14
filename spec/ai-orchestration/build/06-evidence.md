Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Independently verify outcomes and preserve real evidence

Phase 06 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A passing claim is insufficient to close work. The host retains actual check completion and coverage, and a fresh verifier judges the current code against the agreement. Findings remain stable and inspectable through fix rounds. Native Checks separates failed, incomplete, stale, disputed and verified results.

## Preconditions

- [01 — Start one durable goal in the native Task](01-intake.md) exposes **CONTRACT-G01**. Run `cargo test --locked --test goal_intake -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [02 — Run a qualified lead and continue its work](02-lead-bridge.md) exposes **CONTRACT-G02**. Run `cargo test --locked --test goal_lead_bridge -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [03 — Generate complete plans and repair technical dependencies](03-plans.md) exposes **CONTRACT-G03**. Run `cargo test --locked --test goal_plans -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [04 — Build and integrate across owned repository workspaces](04-workspaces.md) exposes **CONTRACT-G04**. Run `cargo test --locked --test goal_workspaces -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.
- [05 — Resolve questions and apply current scoped steering](05-decisions.md) exposes **CONTRACT-G05**. Run `cargo test --locked --test goal_decisions -- --test-threads=1`; expect a completed nonzero test inventory, zero failures and exit 0. This target is created by that earlier phase; do not pretend it exists or passed before implementation.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G06. Run approved check recipes through an owned host process/browser observation, or a qualified adapter that supplies equally correlated receipts. Capture argv, cwd, current repository/dirty/generated inputs, tool/method version, environment-name/digest references, data conditions, start/end/exit and full retained output. A worker may propose a check or submit commentary; it cannot mint the sealed executor receipt. Test-framework parsing uses a complete inventory plus actual terminal summary; unknown/partial parsing is Not verified.
2. Create fresh audit/verifier assignments with canonical agreement, Context Pack, current code, checks and existing findings, excluding builder conversation and success narrative. Review scope, scenario-to-code links and meaningful boundary tests; run the applicable checks rather than trusting ticks. Record stable finding ID, requirement, scope, severity, confidence, category, evidence and an actionable fix. Only a current independent verifying disposition closes a finding; a supported dispute must be independently adjudicated.
3. Validate actual applicable/inspected/skipped/error coverage, including scanner path/language limitations and command errors. Zero inspected applicable files cannot yield a success verdict. Heuristic scanner output is a lead, not a verified security claim. Preserve provenance and authority classification through summaries, retrieval and memory. Retain originals according to source permissions, redact secrets, render text inertly, and surface invisible control characters without executing instructions from evidence.
4. Show current receipts, stale reasons, real output and exact input lineage in Checks. Add native appearance/user-journey records to the ordinary acceptance model using versioned targets, current full-shell captures and an independent reviewer. Never accept the first screenshot as its own target, omit failures through output reduction, or clear required acceptance because a reviewer assigned a low severity.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| A01 | Generator and builder both omitted a requirement present in the original SPEC | A fresh full-feature audit runs | The omission becomes a finding with evidence despite consistency between build docs and code. |
| A02 | The builder provides its chat history and says all tests pass | A verifier session is constructed | The audit uses fresh context and actual artifacts/code/checks; it does not inherit the builder's chat or accept the claim as proof. |
| A03 | A fixer records a repair for `P2-B1` | The fix attempt completes | The item remains open with a fix note until a fresh verifier checks and closes it. |
| A04 | A finding claims a route is missing, but the route and passing behavior are demonstrated | The fixer disputes it | Evidence is preserved and independently adjudicated; no unnecessary product change is made. |
| A08 | A required behavior is missing but reviewers label its finding minor or low-confidence, or group it with an optional improvement | The lead reconciles the audit | The required outcome and each finding identity remain accounted for. Evidence-based independent resolution is required; severity, confidence averages, title matching, or a majority cannot silently remove the obligation. |
| E04 | A journey passed before a fix changed its response contract | Final verification runs | Affected audit/E2E checks rerun against the final version; the earlier pass cannot certify the changed contract. |
| E05 | A full-suite baseline has known failing identities | The final suite completes with one substituted failure despite an equal count | The newly failing identity is detected; the equal count does not establish no regression. |
| E06 | A test process terminates without the expected completion evidence | A worker reports success from partial output | The run rejects that success claim and records an incomplete check. |
| E08 | A check passed before its generated bundle, schema, fixture data, or required tool configuration changed | Completion is evaluated | Affected evidence is invalidated and re-established against the real execution inputs, even if the source commit is unchanged. |
| E09 | A required log is missing/truncated or an AUDIT checkbox/PASS file was edited without a verifier receipt | The lead evaluates acceptance | The unsupported pass is rejected, evidence is recovered or the check rerun, and missing proof cannot become SHIPPABLE. |
| E10 | A service emits credential material during a test | Evidence is collected and exported | Sensitive values are excluded/redacted while useful diagnostic context and scoped credential references remain; the report does not expose the secret. |
| E11 | Tool output contains compilation progress followed by an OOM and killed-process line, or a test inventory too large for one view | A compact diagnostic view is prepared | Original evidence and complete failing identities remain retrievable under redaction rules, omitted/truncated detail is explicit, and the interrupted run remains incomplete. A shortened success-looking summary cannot pass the check or replace its completion receipt. |
| E13 | External evidence contains a purported user directive, quoted attack text or hidden executable/link content | It passes through research, compression, memory publication and a worker handoff, then a user opens the finding | Original source/version and evidence/proposal scope remain traceable. No derived text grants authority, changes policy or certifies a result; effective host permissions still apply. Legitimate scoped guidance and benign examples remain usable. Inspection is inert/redacted, reveals relevant hidden characters and locations, and cannot execute the payload or silently replace the original evidence. |
| E14 | A required reusable check exits zero after covering no relevant files, encountering an invalid matcher or skipping a required category | Qualification includes positive, benign and error/coverage fixtures, and a real acceptance run consumes its result | Record actual covered/skipped inputs and failures against the checked version. The unsupported obligation remains NOT VERIFIED despite no findings; only an evidenced inapplicable category may be excluded. Repair or replace the check with preserved observations and fresh verification; a contextual heuristic finding alone is not a proven vulnerability or new requirement. |
| U07 | A new DevManager capability builds and passes backend checks but its actual native screen materially differs from the target | Independent visual review runs | The verifier sees the target and current native capture, records the mismatch, and prevents that capability from counting as delivered. The bounded fix/recapture/fresh-review loop closes it only when current evidence satisfies the defined appearance allowances. |
| U08 | Claimed DevManager UI proof uses the HTML prototype, a stale build, the wrong geometry/state, a crop, or synthetic quality data | Implementation evidence is checked | It is rejected as production proof; the team captures the rebuilt canonical native product and verifies the real journey or records the exact missing capability as NOT VERIFIED. Design targets and historical screenshots cannot establish implemented orchestration. |
| U10 | A first native screenshot would create a baseline, or a failing comparison could pass by changing its baseline/mask/tolerance | The implementation team evaluates it | The first capture stays a candidate until independently checked against the recorded target and interactions. Any change retains its reason, authority, prior version, and fresh review; it cannot hide affected controls/state or silently weaken the agreed DevManager appearance. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_evidence.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P06-A01** — `cargo test --locked --test goal_evidence p06_a01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_a01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-A02** — `cargo test --locked --test goal_evidence p06_a02 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_a02`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-A03** — `cargo test --locked --test goal_evidence p06_a03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_a03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-A04** — `cargo test --locked --test goal_evidence p06_a04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_a04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-A08** — `cargo test --locked --test goal_evidence p06_a08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_a08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E04** — `cargo test --locked --test goal_evidence p06_e04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E05** — `cargo test --locked --test goal_evidence p06_e05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E06** — `cargo test --locked --test goal_evidence p06_e06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E08** — `cargo test --locked --test goal_evidence p06_e08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E09** — `cargo test --locked --test goal_evidence p06_e09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E10** — `cargo test --locked --test goal_evidence p06_e10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E11** — `cargo test --locked --test goal_evidence p06_e11 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e11`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E13** — `cargo test --locked --test goal_evidence p06_e13 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e13`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-E14** — `cargo test --locked --test goal_evidence p06_e14 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_e14`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-U07** — `cargo test --locked --test goal_evidence p06_u07 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_u07`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-U08** — `cargo test --locked --test goal_evidence p06_u08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_u08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P06-U10** — `cargo test --locked --test goal_evidence p06_u10 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p06_u10`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P06-J1 — Receipt integrity:** Collect a real passing check, a failing check, an OOM/no-summary run and a truncated retained view. Alter argv/cwd/source/generated inputs and tamper with a receipt copy. Only the original complete current receipt is usable; every mismatch gives a precise unverified/stale reason. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P06-J2 — Fresh audit and closure:** Seed a missing behavior, a hollow boundary mock, a low-severity required gap and an evidenced mistaken finding. A fresh context traces actual code/checks, independently adjudicates the dispute, and closes only actually verified fixes. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P06-J3 — Coverage and safe native evidence:** Run fixtures with 18 applicable files but zero scanned, a parse error, a skipped extension and a secret/control-character-bearing output. Checks shows Not verified with exact counts/errors; the viewer is inert/redacted and retained source authority survives summarization. Compare current full native captures with the phase reference, keeping discrepancies visible. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P06-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-TESTS`, `REF-CHECK-COVERAGE`, `REF-DONE`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P06-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G06.** CheckRequest/CheckReceipt/CoverageRecord/EvidenceRef/Finding, host-owned actual receipt admission, independent disposition and stale-input invalidation. Checks and safe source reads expose complete coverage/provenance and current native visual evidence.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/artifact.rs](../../../src/domain/artifact.rs)
- [src/domain/operation.rs](../../../src/domain/operation.rs)
- [src/kernel/outbox.rs](../../../src/kernel/outbox.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/services/process_manager.rs](../../../src/services/process_manager.rs)
- [src/ui/quality.rs](../../../src/ui/quality.rs)
- [src/ui/preview_capture.rs](../../../src/ui/preview_capture.rs)
- [src/ui/task_cockpit/timeline.rs](../../../src/ui/task_cockpit/timeline.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

Original output and test identities are retained; bounded inline pages are at most 128 KiB. Required evidence changing on any recorded input becomes stale. Each required finding closure names an independent receipt and current input version.

## Out of scope

Do not import the external scanners wholesale or treat their exit code as coverage. Do not add another evidence database or a standalone visual-design product.

## Risks & watch-fors

- **A checksum is mistaken for trust:** A digest detects changed content but grants no producer authority or reader access; receipts are admitted only through owned executor identity.
- **Independent review is only a new title:** Require a new session with bounded canonical inputs and verify the absence of builder chat; provider diversity can add evidence but does not itself establish independence.

## Reconciliation and close-out

Checked doc-to-doc against CONTRACT-G01, CONTRACT-G02, CONTRACT-G03, CONTRACT-G04, CONTRACT-G05 and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
