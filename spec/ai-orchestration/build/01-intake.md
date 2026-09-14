Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: Start one durable goal in the native Task

Phase 01 of 17 · Repository: DevManager · Implementation status: NOT STARTED BY THIS PACK.

## Why

A finished spec, a problem report and a project brief enter through one native action. The host records exactly what was handed over, its authority and its project roots before a lead runs. A reconnect or repeated submission attaches to that same logical start. Existing manual Tasks remain usable throughout.

## Preconditions

- Read the current Context Pack and applicable AGENTS.md, inspect the scoped source diff and confirm the command definitions/files below still exist. There are no earlier phase outputs to assume. Apply the isolated checkout preparation in TEST-PLAN before any compiler check.

On a fresh/resumed implementation, also verify the overview contracts and the exact current source/permission versions. Preconditions from this pack are to be run later against actual code. During continuous implementation, reuse current earlier-phase results until inputs change. An unsatisfied precondition blocks only dependent work; record its actual evidence in TRACKING/AUDIT.

## Implementation and native behavior

1. Implement CONTRACT-G01 and the shared types/journal reducer in the overview. Append schema migration 18 with its generated checksum; verify migration 17 is still the last migration before allocating the number. Keep the current Task creation/workspace authorization path, adding one authorized CreateGoal operation that creates the deferred-provider Task and its first run in the same store transaction. Never create an ordinary started provider and later try to adopt it as the lead. Persist the preflight source manifest, path normalization result and idempotency receipt before launch becomes eligible.
2. Resolve the supplied feature directory rather than inferring spec/ versus specs/. Preserve a source status label unless the user actually requested its change. Adopt existing goal/agreement and tracker homes. For an empty project, accept an owned empty directory and materialize the minimal workspace anchor without inventing a remote repository or silently initializing unrelated directories. Record unresolved product decisions as dependent waits; missing mandatory authority never falls back to defaults.
3. Add New goal alongside New manual task in the existing New menu. Submit one outcome, optional rules and the already selected project; show Preparing, queued, actionable missing input and duplicate-start attachment in the same Task conversation. Add the goal query to native/CLI clients and the Connect bridge; capability-aware clients read the same projection. Implement the protocol-v2 negotiation change and explicit older-client incompatibility described in the overview before emitting new event variants.

## Behavior

The following stable SPEC scenarios are reproduced as Given/When/Then acceptance. Scenario text retains the agreed behavior; the implementation steps above settle how it is built.

| ID | Given | When | Then |
| --- | --- | --- | --- |
| H01 | `spec/customer-exports/SPEC.md` is finished but says `DISCOVERY` | Robin says “Do this spec directory end to end” | The lead records the handoff/version and starts; no lock confirmation or status edit is required. Repeat with no status line. |
| H03 | The active workspace contains `spec/exports/`, while an older template says `specs/` | The user hands over `spec/exports/` | The supplied path is used and existing files are preserved; no duplicate feature tree is created. |
| H04 | A prompt mentions policy canaries, but the required imported policy file is missing | The lead prepares the run | It detects the missing source, does not claim the import succeeded, and blocks only work that requires the missing authority. |
| H05 | A project has no Context Pack and a typecheck command is available in its manifest | The lead prepares the run | It records the verified source and actual command result when runnable; unavailable checks are identified, never invented as passes. |
| H06 | An unfinished run already owns a feature directory | Start is repeated through a second client or a differently spelled path to the same directory | The user attaches to the existing run; no duplicate lead, writer, or build-doc tree is admitted. |
| H08 | Build docs, dirty source, and partial TRACKING exist from an earlier session | The feature is handed over again | Existing work and owned processes are reconciled, valid completed work is preserved, and execution resumes from the actual incomplete point. |
| H09 | Required E2E access is absent but project dependencies and source work can be prepared | Preflight runs | Authorized preparation proceeds, user-only missing access is surfaced early once, and unrelated work does not wait for it. |
| N09 | A compact goal grows into a substantial project with a SPEC and expanded tracking | The lead reorganizes artifacts | Requirement/progress authority transfers with versions and references; no two independently editable copies remain authoritative. |
| N12 | An authorized empty workspace and project brief contain enough product behavior to begin | Robin requests that project end to end | The lead chooses permitted technical defaults, scaffolds required source/local setup, establishes real commands, and proceeds through milestones and verification; no pre-existing spec, code, or Context Pack is demanded. |
| U13 | The new native goal entry is available alongside existing manual Tasks | The user submits a spec, brief, slow-page problem, or test campaign | The same scoped goal conversation shows resolved context, preparation and progress without a workflow/team wizard. Duplicate delivery attaches to the current run; existing manual task creation and provider input retain their own behavior. |

## Acceptance checks

Create the behavioral integration target **`tests/goal_intake.rs`** as this phase’s test deliverable. It does not exist merely because this document names it. Tests must drive production command, store/outbox, adapter or client handlers at the relevant boundary, using owned disposable resources and hostile/negative inputs. They must not substitute a handcrafted successful projection for the production result.

- [ ] **P01-H01** — `cargo test --locked --test goal_intake p01_h01 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h01`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-H03** — `cargo test --locked --test goal_intake p01_h03 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h03`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-H04** — `cargo test --locked --test goal_intake p01_h04 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h04`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-H05** — `cargo test --locked --test goal_intake p01_h05 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h05`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-H06** — `cargo test --locked --test goal_intake p01_h06 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h06`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-H08** — `cargo test --locked --test goal_intake p01_h08 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h08`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-H09** — `cargo test --locked --test goal_intake p01_h09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_h09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-N09** — `cargo test --locked --test goal_intake p01_n09 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_n09`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-N12** — `cargo test --locked --test goal_intake p01_n12 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_n12`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.
- [ ] **P01-U13** — `cargo test --locked --test goal_intake p01_u13 -- --exact --test-threads=1`: implement the Given/When/Then above in top-level test `p01_u13`; expect exactly one selected test, its full completion summary, no failure and exit 0. Retain actual output/receipt and input versions. Assertions cover the stated outcome and forbidden false-success path.

The one-test name is a stable acceptance entry point, not permission to mirror implementation internals. Where the scenario requires a real configured provider, browser, native capture or real project, the deterministic test covers the host boundary and **the corresponding live/native check below is additionally mandatory**. An unavailable live prerequisite leaves that scenario Not verified; a fixture pass cannot replace it.

- [ ] **P01-J1 — Start and restart:** Submit a finished DISCOVERY spec, lose the reply, resubmit the same command, then restart the host. Observe one Task, one run, the original source version and one pending launch. Repeat with a new logical request containing identical text and verify it is separately represented, subject to the active-source lease. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P01-J2 — Authority and workspace:** Exercise a missing imported canary, an alias to the same directory, a non-Git root with two nested repos and a genuinely empty owned directory. Check canonical paths, exact blocked dependency and preservation of every existing file. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P01-J3 — Native and compatibility:** Use New goal and New manual task in the rebuilt shell; switch back and forth. Verify selected project, inherited rules, preparation error and retry. A v1 client fails explicitly at handshake before receiving a v2 event; rebuilt native/CLI/Connect clients retain ordinary Task access. Record actual actions, observations, current input versions and resulting artifact/receipt IDs.
- [ ] **P01-UI:** Open the actual rebuilt native entry described above and compare full-shell current captures against `REF-START`, `REF-RUN`. Use the overview’s exact loading/empty/failure/recovery variants, shared components and manifest productRegion/geometry. Independently adjudicate material differences; also test long text, keyboard/focus, wrong-task/late-result protection and the preserved composer. Do not count the reference HTML or a preview-only snapshot as native acceptance.
- [ ] **P01-GATES:** Complete the applicable shared verification/landing gates below; current results are linked to all scenarios they cover.

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

**CONTRACT-G01.** CreateGoal/FollowUp admission, GoalKey/GoalFence/SourceRef, atomic Task/run receipt, GoalRun/source projections, migration/rebuild and protocol-v2 GoalOrchestration bit 20. Public start/follow-up/read actions expose current source/authority and preserve manual Tasks.

This paragraph is identical to the overview contract register. All payloads, limits, errors, identity and evidence rules are defined there; this phase must not reshape them. A required contract change is reconciled with the overview and every dependent doc before code builds on it.

## Context

Feature-specific integration points, verified during generation:

- [src/domain/command.rs](../../../src/domain/command.rs)
- [src/domain/event.rs](../../../src/domain/event.rs)
- [src/domain/id.rs](../../../src/domain/id.rs)
- [src/domain/query.rs](../../../src/domain/query.rs)
- [src/kernel/schema.rs](../../../src/kernel/schema.rs)
- [src/kernel/store.rs](../../../src/kernel/store.rs)
- [src/workspace/model.rs](../../../src/workspace/model.rs)
- [src/workspace/service.rs](../../../src/workspace/service.rs)
- [src/ui/native_shell.rs](../../../src/ui/native_shell.rs)
- [src/ui/task_cockpit/composer.rs](../../../src/ui/task_cockpit/composer.rs)
- [src/protocol/capabilities.rs](../../../src/protocol/capabilities.rs)

All code lands in DevManager; target-project repositories are exercised through the existing workspace boundary, not committed as part of this application phase. Current module boundaries are examples to extend; do not relocate unrelated code to match a speculative architecture.

## Constraints

One active run per Task; one active owning run per canonical handed-off directory. Zero provider launches before durable acceptance. Use the shared 64 KiB request and 200-item query limits.

## Out of scope

Do not alter target-project product requirements or start implementation for a docs-only handoff. Do not refactor manual Task composition.

## Risks & watch-fors

- **Concurrent source edits:** Hash and version sources at admission; compare before publishing a derived file and invalidate affected work on a changed source.
- **Protocol upgrade:** Ship matching app/host/Connect/WASM artifacts together; refuse older clients and older readers of the migrated store clearly, rather than dropping unfamiliar durable facts.

## Reconciliation and close-out

Checked doc-to-doc against the inspected current host/domain/workspace contracts and the locked SPEC/UX chapter. No earlier implementation or successful future precondition is assumed. No project tracker item was silently added to product scope. All technical calls and mitigations raised for this phase are recorded here or in the overview. There are no unresolved product questions or BLOCKERS in this phase document.
